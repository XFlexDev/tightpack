//! Container layout: header, compressed blocks, compressed index, trailer.
//! See FORMAT.md for the byte level description.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;

use crate::codec;
pub use crate::codec::Level;
use crate::node::{Class, Node, RefSource, STREAMS};
use crate::transform;
use crate::wire::{Cur, put_bytes, put_iv, put_uv};

pub const MAGIC: &[u8; 8] = b"TPAK\r\n\x1a\n";
pub const TRAILER_MAGIC: &[u8; 4] = b"TPKI";
pub const VERSION: u16 = 1;
const HEADER_LEN: u64 = 16;
const TRAILER_LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryKind {
    File = 0,
    Dir = 1,
    Symlink = 2,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    pub mode: u32,
    pub mtime: i64,
    pub size: u64,
    pub hash: [u8; 32],
    pub target: String,
    pub node: Node,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    pub stream: Class,
    pub codec: u8,
    pub offset: u64,
    pub comp_len: u64,
    pub raw_len: u64,
    pub crc: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PackOptions {
    pub level: Level,
    pub transforms: transform::Options,
}

/// File handed to [`Writer::add_files`].
pub struct InputFile {
    pub path: String,
    pub mode: u32,
    pub mtime: i64,
    pub data: Vec<u8>,
}

pub struct Writer<W: Write> {
    out: W,
    pos: u64,
    opts: PackOptions,
    bufs: [Vec<u8>; STREAMS],
    logical: [u64; STREAMS],
    blocks: Vec<BlockInfo>,
    entries: Vec<Entry>,
    seen: HashMap<[u8; 32], u64>,
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W, opts: PackOptions) -> Result<Self> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&[0u8; 6])?;
        Ok(Self {
            out,
            pos: HEADER_LEN,
            opts,
            bufs: Default::default(),
            logical: [0; STREAMS],
            blocks: Vec::new(),
            entries: Vec::new(),
            seen: HashMap::new(),
        })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn add_dir(&mut self, path: String, mode: u32, mtime: i64) {
        self.entries.push(Entry {
            path,
            kind: EntryKind::Dir,
            mode,
            mtime,
            size: 0,
            hash: [0; 32],
            target: String::new(),
            node: Node::Empty,
        });
    }

    pub fn add_symlink(&mut self, path: String, target: String, mode: u32, mtime: i64) {
        self.entries.push(Entry {
            path,
            kind: EntryKind::Symlink,
            mode,
            mtime,
            size: 0,
            hash: [0; 32],
            target,
            node: Node::Empty,
        });
    }

    /// Analyze a batch of files in parallel and append them in the given order.
    pub fn add_files(&mut self, files: Vec<InputFile>) -> Result<()> {
        let t = self.opts.transforms;
        let analyzed: Vec<(InputFile, [u8; 32], Option<Node>)> = files
            .into_par_iter()
            .map(|f| {
                let hash = *blake3::hash(&f.data).as_bytes();
                (f, hash, None)
            })
            .collect();
        // Skip work for duplicates (inside this batch or earlier ones).
        let mut first_in_batch: HashMap<[u8; 32], usize> = HashMap::new();
        let mut todo = Vec::new();
        for (i, (_, h, _)) in analyzed.iter().enumerate() {
            if !self.seen.contains_key(h) && first_in_batch.insert(*h, i).is_none() {
                todo.push(i);
            }
        }
        let nodes: Vec<(usize, Node)> = todo
            .par_iter()
            .map(|&i| (i, transform::analyze(&analyzed[i].0.data, &t)))
            .collect();
        let mut analyzed = analyzed;
        for (i, n) in nodes {
            analyzed[i].2 = Some(n);
        }
        for (f, hash, node) in analyzed {
            let size = f.data.len() as u64;
            drop(f.data);
            let node = match (self.seen.get(&hash), node) {
                (Some(&idx), _) => Node::SameAs(idx),
                (None, Some(n)) => self.place(n)?,
                (None, None) => bail!("internal: missing analysis"),
            };
            if !matches!(node, Node::SameAs(_)) && size > 0 {
                self.seen.insert(hash, self.entries.len() as u64);
            }
            self.entries.push(Entry {
                path: f.path,
                kind: EntryKind::File,
                mode: f.mode,
                mtime: f.mtime,
                size,
                hash,
                target: String::new(),
                node,
            });
        }
        Ok(())
    }

    /// Stream a large file straight into a stream without transforms.
    pub fn add_large_file(
        &mut self,
        path: String,
        mode: u32,
        mtime: i64,
        mut r: impl Read,
    ) -> Result<()> {
        let mut hasher = blake3::Hasher::new();
        let mut chunk = vec![0u8; 8 << 20];
        let mut size = 0u64;
        let mut class = None;
        let mut start = 0;
        loop {
            let n = read_full(&mut r, &mut chunk)?;
            if n == 0 {
                break;
            }
            let data = &chunk[..n];
            hasher.update(data);
            let c = *class.get_or_insert_with(|| transform::classify(data));
            if size == 0 {
                start = self.logical[c as usize];
            }
            size += n as u64;
            self.append(c, data)?;
        }
        let hash = *hasher.finalize().as_bytes();
        let node = match (self.seen.get(&hash), class) {
            (Some(&i), _) => Node::SameAs(i),
            (None, Some(c)) => Node::Ref {
                stream: c,
                off: start,
                len: size,
            },
            (None, None) => Node::Empty,
        };
        if matches!(node, Node::Ref { .. }) {
            self.seen.insert(hash, self.entries.len() as u64);
        }
        self.entries.push(Entry {
            path,
            kind: EntryKind::File,
            mode,
            mtime,
            size,
            hash,
            target: String::new(),
            node,
        });
        Ok(())
    }

    fn place(&mut self, mut node: Node) -> Result<Node> {
        node.walk_mut(&mut |n| {
            if let Node::Bytes(class, data) = n {
                let class = *class;
                let off = self.logical[class as usize];
                let len = data.len() as u64;
                let data = std::mem::take(data);
                self.append(class, &data)?;
                *n = Node::Ref {
                    stream: class,
                    off,
                    len,
                };
            }
            Ok(())
        })?;
        Ok(node)
    }

    fn append(&mut self, class: Class, mut data: &[u8]) -> Result<()> {
        let bs = self.opts.level.block_size();
        let i = class as usize;
        self.logical[i] += data.len() as u64;
        while !data.is_empty() {
            let room = bs - self.bufs[i].len();
            let take = room.min(data.len());
            self.bufs[i].extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.bufs[i].len() >= bs {
                self.flush(class)?;
            }
        }
        Ok(())
    }

    fn flush(&mut self, class: Class) -> Result<()> {
        let raw = std::mem::take(&mut self.bufs[class as usize]);
        if raw.is_empty() {
            return Ok(());
        }
        let (codec, comp) = codec::compress(class, self.opts.level, &raw)?;
        self.out.write_all(&comp)?;
        self.blocks.push(BlockInfo {
            stream: class,
            codec,
            offset: self.pos,
            comp_len: comp.len() as u64,
            raw_len: raw.len() as u64,
            crc: crc32fast::hash(&raw),
        });
        self.pos += comp.len() as u64;
        Ok(())
    }

    /// Flush all streams, write the index and trailer. Returns the sink and the archive size.
    pub fn finish(mut self) -> Result<(W, u64)> {
        for c in [Class::Data, Class::Coded, Class::Meta] {
            self.flush(c)?;
        }
        let idx = encode_index(&self.blocks, &self.entries);
        let comp = zstd::bulk::compress(&idx, 19)?;
        let idx_off = self.pos;
        self.out.write_all(&comp)?;
        let mut t = Vec::with_capacity(TRAILER_LEN);
        t.extend(idx_off.to_le_bytes());
        t.extend((comp.len() as u64).to_le_bytes());
        t.extend((idx.len() as u64).to_le_bytes());
        t.extend(crc32fast::hash(&comp).to_le_bytes());
        t.extend(TRAILER_MAGIC);
        self.out.write_all(&t)?;
        self.out.flush()?;
        let total = self.pos + comp.len() as u64 + TRAILER_LEN as u64;
        Ok((self.out, total))
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn encode_index(blocks: &[BlockInfo], entries: &[Entry]) -> Vec<u8> {
    let mut o = Vec::new();
    put_uv(&mut o, u64::from(VERSION));
    put_uv(&mut o, blocks.len() as u64);
    for b in blocks {
        o.push(b.stream as u8);
        o.push(b.codec);
        put_uv(&mut o, b.offset);
        put_uv(&mut o, b.comp_len);
        put_uv(&mut o, b.raw_len);
        o.extend(b.crc.to_le_bytes());
    }
    put_uv(&mut o, entries.len() as u64);
    for e in entries {
        o.push(e.kind as u8);
        put_bytes(&mut o, e.path.as_bytes());
        put_uv(&mut o, u64::from(e.mode));
        put_iv(&mut o, e.mtime);
        match e.kind {
            EntryKind::File => {
                put_uv(&mut o, e.size);
                o.extend_from_slice(&e.hash);
                e.node.encode(&mut o);
            }
            EntryKind::Symlink => put_bytes(&mut o, e.target.as_bytes()),
            EntryKind::Dir => {}
        }
    }
    o
}

fn decode_index(idx: &[u8]) -> Result<(Vec<BlockInfo>, Vec<Entry>)> {
    let mut c = Cur::new(idx);
    let v = c.uv()?;
    ensure!(v == u64::from(VERSION), "unsupported index version {v}");
    let nb = c.len_u()?;
    ensure!(nb <= idx.len(), "bad block count");
    let mut blocks = Vec::with_capacity(nb);
    for _ in 0..nb {
        let stream = Class::from_u8(c.u8()?)?;
        let codec = c.u8()?;
        let offset = c.uv()?;
        let comp_len = c.uv()?;
        let raw_len = c.uv()?;
        let crc = u32::from_le_bytes(c.take(4)?.try_into()?);
        blocks.push(BlockInfo {
            stream,
            codec,
            offset,
            comp_len,
            raw_len,
            crc,
        });
    }
    let ne = c.len_u()?;
    ensure!(ne <= idx.len(), "bad entry count");
    let mut entries = Vec::with_capacity(ne);
    for i in 0..ne {
        let kind = match c.u8()? {
            0 => EntryKind::File,
            1 => EntryKind::Dir,
            2 => EntryKind::Symlink,
            k => bail!("unknown entry kind {k}"),
        };
        let path = String::from_utf8(c.bytes()?.to_vec()).context("path is not UTF-8")?;
        let mode = u32::try_from(c.uv()?)?;
        let mtime = c.iv()?;
        let mut e = Entry {
            path,
            kind,
            mode,
            mtime,
            size: 0,
            hash: [0; 32],
            target: String::new(),
            node: Node::Empty,
        };
        match kind {
            EntryKind::File => {
                e.size = c.uv()?;
                e.hash = c.take(32)?.try_into()?;
                e.node = Node::decode(&mut c, 0)?;
                if let Node::SameAs(j) = e.node {
                    ensure!((j as usize) < i, "SameAs must point backwards");
                }
            }
            EntryKind::Symlink => e.target = String::from_utf8(c.bytes()?.to_vec())?,
            EntryKind::Dir => {}
        }
        entries.push(e);
    }
    Ok((blocks, entries))
}

struct Store<R> {
    file: R,
    blocks: Vec<BlockInfo>,
    /// Per stream: (logical start, index into `blocks`).
    map: [Vec<(u64, usize)>; STREAMS],
    cache: Vec<(usize, Arc<Vec<u8>>)>,
}

impl<R: Read + Seek> Store<R> {
    fn block(&mut self, i: usize) -> Result<Arc<Vec<u8>>> {
        if let Some(p) = self.cache.iter().position(|(b, _)| *b == i) {
            let hit = self.cache.remove(p);
            self.cache.push(hit.clone());
            return Ok(hit.1);
        }
        let b = self.blocks[i];
        self.file.seek(SeekFrom::Start(b.offset))?;
        let mut comp = vec![0u8; usize::try_from(b.comp_len)?];
        self.file
            .read_exact(&mut comp)
            .context("archive truncated")?;
        let raw = codec::decompress(b.codec, &comp, usize::try_from(b.raw_len)?)?;
        ensure!(
            crc32fast::hash(&raw) == b.crc,
            "block {i} failed its checksum"
        );
        let raw = Arc::new(raw);
        if self.cache.len() >= 4 {
            self.cache.remove(0);
        }
        self.cache.push((i, raw.clone()));
        Ok(raw)
    }
}

impl<R: Read + Seek> RefSource for Store<R> {
    fn read_ref(
        &mut self,
        stream: Class,
        mut off: u64,
        mut len: u64,
        out: &mut dyn Write,
    ) -> Result<()> {
        while len > 0 {
            let map = &self.map[stream as usize];
            let k = map.partition_point(|&(start, _)| start <= off);
            ensure!(k > 0, "reference before stream start");
            let (start, bi) = map[k - 1];
            let raw_len = self.blocks[bi].raw_len;
            ensure!(off - start < raw_len, "reference past stream end");
            let block = self.block(bi)?;
            let from = (off - start) as usize;
            let n = (raw_len - (off - start)).min(len) as usize;
            out.write_all(&block[from..from + n])?;
            off += n as u64;
            len -= n as u64;
        }
        Ok(())
    }
}

pub struct Reader<R> {
    store: Store<R>,
    pub entries: Vec<Entry>,
    pub archive_len: u64,
}

struct HashingWriter<'a> {
    inner: &'a mut dyn Write,
    hasher: blake3::Hasher,
    n: u64,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.inner.write(buf)?;
        self.hasher.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<R: Read + Seek> Reader<R> {
    pub fn open(mut file: R) -> Result<Self> {
        let len = file.seek(SeekFrom::End(0))?;
        ensure!(
            len >= HEADER_LEN + TRAILER_LEN as u64,
            "not a tightpack archive (too small)"
        );
        file.seek(SeekFrom::Start(0))?;
        let mut h = [0u8; 16];
        file.read_exact(&mut h)?;
        ensure!(&h[..8] == MAGIC, "not a tightpack archive");
        let ver = u16::from_le_bytes([h[8], h[9]]);
        ensure!(ver == VERSION, "unsupported archive version {ver}");
        file.seek(SeekFrom::Start(len - TRAILER_LEN as u64))?;
        let mut t = [0u8; TRAILER_LEN];
        file.read_exact(&mut t)?;
        ensure!(
            &t[28..] == TRAILER_MAGIC,
            "archive trailer missing (truncated file?)"
        );
        let u = |i: usize| u64::from_le_bytes(t[i..i + 8].try_into().unwrap());
        let (off, clen, rlen) = (u(0), u(8), u(16));
        ensure!(
            off.checked_add(clen) == Some(len - TRAILER_LEN as u64),
            "bad index location"
        );
        ensure!(rlen < 1 << 32, "index too large");
        file.seek(SeekFrom::Start(off))?;
        let mut comp = vec![0u8; usize::try_from(clen)?];
        file.read_exact(&mut comp)?;
        ensure!(
            crc32fast::hash(&comp) == u32::from_le_bytes(t[24..28].try_into()?),
            "index checksum mismatch"
        );
        let idx = zstd::bulk::decompress(&comp, usize::try_from(rlen)?)?;
        let (blocks, entries) = decode_index(&idx)?;
        let mut map: [Vec<(u64, usize)>; STREAMS] = Default::default();
        let mut logical = [0u64; STREAMS];
        for (i, b) in blocks.iter().enumerate() {
            ensure!(
                b.offset >= HEADER_LEN && b.offset.saturating_add(b.comp_len) <= off,
                "block {i} out of bounds"
            );
            map[b.stream as usize].push((logical[b.stream as usize], i));
            logical[b.stream as usize] += b.raw_len;
        }
        Ok(Self {
            store: Store {
                file,
                blocks,
                map,
                cache: Vec::new(),
            },
            entries,
            archive_len: len,
        })
    }

    pub fn blocks(&self) -> &[BlockInfo] {
        &self.store.blocks
    }

    /// Write the exact original bytes of file entry `i`, verifying size and BLAKE3 hash.
    pub fn extract(&mut self, i: usize, out: &mut dyn Write) -> Result<()> {
        let e = &self.entries[i];
        ensure!(e.kind == EntryKind::File, "{} is not a file", e.path);
        let (size, hash, path) = (e.size, e.hash, e.path.clone());
        let node = match &e.node {
            Node::SameAs(j) => self.entries[*j as usize].node.clone(),
            n => n.clone(),
        };
        let mut w = HashingWriter {
            inner: out,
            hasher: blake3::Hasher::new(),
            n: 0,
        };
        node.write(&mut self.store, &mut w)
            .with_context(|| format!("rebuilding {path}"))?;
        ensure!(w.n == size, "{path}: size mismatch");
        ensure!(
            *w.hasher.finalize().as_bytes() == hash,
            "{path}: hash mismatch"
        );
        Ok(())
    }
}

/// Counts of transforms applied across the entries.
#[derive(Debug, Default, Clone, Copy)]
pub struct TransformStats {
    pub jpeg: u64,
    pub deflate: u64,
    pub png: u64,
    pub pcm: u64,
    pub gif: u64,
    pub bcj: u64,
    pub zstd: u64,
    pub dedup: u64,
}

pub fn transform_stats(entries: &[Entry]) -> TransformStats {
    let mut s = TransformStats::default();
    for e in entries {
        e.node.walk(&mut |n| match n {
            Node::Lepton(_) => s.jpeg += 1,
            Node::Deflate { .. } => s.deflate += 1,
            Node::PngIdat { .. } => s.png += 1,
            Node::Pcm { .. } => s.pcm += 1,
            Node::Gif { .. } => s.gif += 1,
            Node::Bcj { .. } => s.bcj += 1,
            Node::ZstdFrame { .. } => s.zstd += 1,
            Node::SameAs(_) => s.dedup += 1,
            _ => {}
        });
    }
    s
}
