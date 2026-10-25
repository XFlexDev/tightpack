//! Recipes: a small tree that describes how to rebuild the exact original
//! bytes of a file from the archive's streams. Encoding is specified in
//! FORMAT.md.

use std::io::{Cursor, Write};

use anyhow::{Context, Result, bail, ensure};

use crate::wire::{Cur, put_uv};

/// Which logical stream a piece of data is appended to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Class {
    /// Compressible payload: plain text, decompressed deflate data, raw files.
    Data = 0,
    /// Already entropy coded payload: Lepton, FLAC, MP3, video, unknown dense data.
    Coded = 1,
    /// Small structural bytes: container headers and deflate corrections.
    Meta = 2,
}

pub const STREAMS: usize = 3;

impl Class {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Self::Data,
            1 => Self::Coded,
            2 => Self::Meta,
            _ => bail!("unknown stream {v}"),
        })
    }
}

pub const TAG_REF: u8 = 0;
pub const TAG_CONCAT: u8 = 1;
pub const TAG_DEFLATE: u8 = 2;
pub const TAG_LEPTON: u8 = 3;
pub const TAG_PCM: u8 = 4;
pub const TAG_PNG_IDAT: u8 = 5;
pub const TAG_SAME_AS: u8 = 6;
pub const TAG_EMPTY: u8 = 7;
pub const TAG_BCJ: u8 = 8;
pub const TAG_GIF: u8 = 9;
pub const TAG_ZSTD_FRAME: u8 = 10;

pub const PCM_BIG_ENDIAN: u8 = 1;
pub const PCM_UNSIGNED: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// Bytes not yet written to a stream (only exists while packing).
    Bytes(Class, Vec<u8>),
    /// A byte range of a logical stream.
    Ref {
        stream: Class,
        off: u64,
        len: u64,
    },
    Concat(Vec<Node>),
    /// Re-create a raw DEFLATE bitstream from its plain text plus preflate corrections.
    Deflate {
        plain: Box<Node>,
        corrections: Box<Node>,
    },
    /// Decode a Lepton payload back to the original JPEG file.
    Lepton(Box<Node>),
    /// Decode FLAC frames to interleaved integer PCM.
    Pcm {
        bits: u8,
        flags: u8,
        flac: Box<Node>,
    },
    /// Split bytes into PNG IDAT chunks of the given sizes (adds length, type and CRC).
    PngIdat {
        sizes: Vec<u32>,
        data: Box<Node>,
    },
    /// Identical to an earlier entry.
    SameAs(u64),
    /// x86 call/jump addresses stored as absolute positions (BCJ); the inner
    /// recipe produces the filtered bytes.
    Bcj {
        arch: u8,
        data: Box<Node>,
    },
    /// A GIF image's LZW payload decoded to index pixels, re-encoded on unpack.
    /// `sizes` are the original sub-block lengths.
    Gif {
        min_code_size: u8,
        sizes: Vec<u32>,
        pixels: Box<Node>,
    },
    /// Re-create a zstd frame by recompressing the plain recipe at `level`.
    ZstdFrame {
        level: i32,
        plain: Box<Node>,
    },
    Empty,
}

pub trait RefSource {
    fn read_ref(&mut self, stream: Class, off: u64, len: u64, out: &mut dyn Write) -> Result<()>;
}

/// Source for recipes that only contain in-memory bytes.
pub struct NoRefs;

impl RefSource for NoRefs {
    fn read_ref(&mut self, _: Class, _: u64, _: u64, _: &mut dyn Write) -> Result<()> {
        bail!("unexpected stream reference")
    }
}

impl Node {
    pub fn raw(class: Class, data: &[u8]) -> Self {
        if data.is_empty() {
            Self::Empty
        } else {
            Self::Bytes(class, data.to_vec())
        }
    }

    pub fn concat(mut parts: Vec<Node>) -> Self {
        parts.retain(|p| *p != Node::Empty);
        match parts.len() {
            0 => Self::Empty,
            1 => parts.pop().unwrap(),
            _ => Self::Concat(parts),
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Node::Bytes(..) => panic!("pending bytes must be flushed before encoding"),
            Node::Ref { stream, off, len } => {
                out.push(TAG_REF);
                out.push(*stream as u8);
                put_uv(out, *off);
                put_uv(out, *len);
            }
            Node::Concat(parts) => {
                out.push(TAG_CONCAT);
                put_uv(out, parts.len() as u64);
                for p in parts {
                    p.encode(out);
                }
            }
            Node::Deflate { plain, corrections } => {
                out.push(TAG_DEFLATE);
                plain.encode(out);
                corrections.encode(out);
            }
            Node::Lepton(n) => {
                out.push(TAG_LEPTON);
                n.encode(out);
            }
            Node::Pcm { bits, flags, flac } => {
                out.push(TAG_PCM);
                out.push(*bits);
                out.push(*flags);
                flac.encode(out);
            }
            Node::PngIdat { sizes, data } => {
                out.push(TAG_PNG_IDAT);
                put_uv(out, sizes.len() as u64);
                for s in sizes {
                    put_uv(out, u64::from(*s));
                }
                data.encode(out);
            }
            Node::Bcj { arch, data } => {
                out.push(TAG_BCJ);
                out.push(*arch);
                data.encode(out);
            }
            Node::Gif {
                min_code_size,
                sizes,
                pixels,
            } => {
                out.push(TAG_GIF);
                out.push(*min_code_size);
                put_uv(out, sizes.len() as u64);
                for s in sizes {
                    put_uv(out, u64::from(*s));
                }
                pixels.encode(out);
            }
            Node::ZstdFrame { level, plain } => {
                out.push(TAG_ZSTD_FRAME);
                put_uv(out, *level as u64);
                plain.encode(out);
            }
            Node::SameAs(i) => {
                out.push(TAG_SAME_AS);
                put_uv(out, *i);
            }
            Node::Empty => out.push(TAG_EMPTY),
        }
    }

    pub fn decode(c: &mut Cur, depth: u32) -> Result<Self> {
        ensure!(depth < 64, "recipe nested too deeply");
        let d = depth + 1;
        Ok(match c.u8()? {
            TAG_REF => Node::Ref {
                stream: Class::from_u8(c.u8()?)?,
                off: c.uv()?,
                len: c.uv()?,
            },
            TAG_CONCAT => {
                let n = c.len_u()?;
                ensure!(n <= c.buf.len(), "bad concat length");
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(Node::decode(c, d)?);
                }
                Node::Concat(v)
            }
            TAG_DEFLATE => Node::Deflate {
                plain: Box::new(Node::decode(c, d)?),
                corrections: Box::new(Node::decode(c, d)?),
            },
            TAG_LEPTON => Node::Lepton(Box::new(Node::decode(c, d)?)),
            TAG_PCM => Node::Pcm {
                bits: c.u8()?,
                flags: c.u8()?,
                flac: Box::new(Node::decode(c, d)?),
            },
            TAG_PNG_IDAT => {
                let n = c.len_u()?;
                ensure!(n <= c.buf.len(), "bad chunk count");
                let mut sizes = Vec::with_capacity(n);
                for _ in 0..n {
                    sizes.push(u32::try_from(c.uv()?)?);
                }
                Node::PngIdat {
                    sizes,
                    data: Box::new(Node::decode(c, d)?),
                }
            }
            TAG_BCJ => Node::Bcj {
                arch: c.u8()?,
                data: Box::new(Node::decode(c, d)?),
            },
            TAG_GIF => {
                let min_code_size = c.u8()?;
                let n = c.len_u()?;
                ensure!(n <= c.buf.len(), "bad chunk count");
                let mut sizes = Vec::with_capacity(n);
                for _ in 0..n {
                    sizes.push(u32::try_from(c.uv()?)?);
                }
                Node::Gif {
                    min_code_size,
                    sizes,
                    pixels: Box::new(Node::decode(c, d)?),
                }
            }
            TAG_ZSTD_FRAME => Node::ZstdFrame {
                level: i32::try_from(c.uv()?)?,
                plain: Box::new(Node::decode(c, d)?),
            },
            TAG_SAME_AS => Node::SameAs(c.uv()?),
            TAG_EMPTY => Node::Empty,
            t => bail!("unknown recipe tag {t}"),
        })
    }

    /// Visit every node, depth first.
    pub fn walk(&self, f: &mut dyn FnMut(&Node)) {
        f(self);
        match self {
            Node::Concat(p) => p.iter().for_each(|n| n.walk(f)),
            Node::Deflate { plain, corrections } => {
                plain.walk(f);
                corrections.walk(f);
            }
            Node::Lepton(n)
            | Node::Pcm { flac: n, .. }
            | Node::PngIdat { data: n, .. }
            | Node::Bcj { data: n, .. }
            | Node::Gif { pixels: n, .. }
            | Node::ZstdFrame { plain: n, .. } => n.walk(f),
            _ => {}
        }
    }

    pub fn walk_mut(&mut self, f: &mut dyn FnMut(&mut Node) -> Result<()>) -> Result<()> {
        f(self)?;
        match self {
            Node::Concat(p) => {
                for n in p {
                    n.walk_mut(f)?;
                }
            }
            Node::Deflate { plain, corrections } => {
                plain.walk_mut(f)?;
                corrections.walk_mut(f)?;
            }
            Node::Lepton(n)
            | Node::Pcm { flac: n, .. }
            | Node::PngIdat { data: n, .. }
            | Node::Bcj { data: n, .. }
            | Node::Gif { pixels: n, .. }
            | Node::ZstdFrame { plain: n, .. } => n.walk_mut(f)?,
            _ => {}
        }
        Ok(())
    }

    pub fn materialize(&self, src: &mut dyn RefSource) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.write(src, &mut v)?;
        Ok(v)
    }

    /// Rebuild the original bytes described by this node.
    pub fn write(&self, src: &mut dyn RefSource, out: &mut dyn Write) -> Result<()> {
        match self {
            Node::Bytes(_, b) => out.write_all(b)?,
            Node::Ref { stream, off, len } => src.read_ref(*stream, *off, *len, out)?,
            Node::Concat(parts) => {
                for p in parts {
                    p.write(src, out)?;
                }
            }
            Node::Deflate { plain, corrections } => {
                let plain = plain.materialize(src)?;
                let corr = corrections.materialize(src)?;
                let bytes = preflate_rs::recreate_whole_deflate_stream(&plain, &corr)
                    .map_err(|e| anyhow::anyhow!("deflate rebuild failed: {e:?}"))?;
                out.write_all(&bytes)?;
            }
            Node::Lepton(n) => {
                let lep = n.materialize(src)?;
                let mut jpg = Vec::with_capacity(lep.len() * 5 / 4);
                lepton_jpeg::decode_lepton(
                    &mut Cursor::new(&lep),
                    &mut jpg,
                    &lepton_jpeg::EnabledFeatures::compat_lepton_vector_read(),
                    &lepton_jpeg::DEFAULT_THREAD_POOL,
                )
                .map_err(|e| anyhow::anyhow!("lepton decode failed: {e:?}"))?;
                out.write_all(&jpg)?;
            }
            Node::Pcm { bits, flags, flac } => {
                let data = flac.materialize(src)?;
                write_pcm(&data, *bits, *flags, out)?;
            }
            Node::PngIdat { sizes, data } => {
                let data = data.materialize(src)?;
                let total: u64 = sizes.iter().map(|s| u64::from(*s)).sum();
                ensure!(total == data.len() as u64, "IDAT sizes do not match data");
                let mut pos = 0usize;
                for &s in sizes {
                    let chunk = &data[pos..pos + s as usize];
                    pos += s as usize;
                    out.write_all(&s.to_be_bytes())?;
                    let mut h = crc32fast::Hasher::new();
                    h.update(b"IDAT");
                    h.update(chunk);
                    out.write_all(b"IDAT")?;
                    out.write_all(chunk)?;
                    out.write_all(&h.finalize().to_be_bytes())?;
                }
            }
            Node::Bcj { arch, data } => {
                ensure!(*arch == 1, "unsupported BCJ architecture {arch}");
                let mut filtered = data.materialize(src)?;
                crate::filters::bcj_x86_decode(&mut filtered);
                out.write_all(&filtered)?;
            }
            Node::Gif {
                min_code_size,
                sizes,
                pixels,
            } => {
                let px = pixels.materialize(src)?;
                let lzw = crate::filters::lzw_encode(*min_code_size, &px);
                let total: u64 = sizes.iter().map(|s| u64::from(*s)).sum();
                ensure!(total == lzw.len() as u64, "GIF subblock sizes mismatch");
                out.write_all(&[*min_code_size])?;
                let mut pos = 0usize;
                for &s in sizes {
                    out.write_all(&[s as u8])?;
                    out.write_all(&lzw[pos..pos + s as usize])?;
                    pos += s as usize;
                }
                out.write_all(&[0])?;
            }
            Node::ZstdFrame { level, plain } => {
                let p = plain.materialize(src)?;
                let b = zstd::bulk::compress(&p, *level).context("zstd recompression failed")?;
                out.write_all(&b)?;
            }
            Node::SameAs(_) => bail!("SameAs is only valid as a top level recipe"),
            Node::Empty => {}
        }
        Ok(())
    }
}

fn write_pcm(flac: &[u8], bits: u8, flags: u8, out: &mut dyn Write) -> Result<()> {
    let mut r = claxon::FlacReader::new(Cursor::new(flac)).context("bad FLAC payload")?;
    ensure!(
        u32::from(bits) == r.streaminfo().bits_per_sample,
        "PCM bit depth mismatch"
    );
    let be = flags & PCM_BIG_ENDIAN != 0;
    let unsigned = flags & PCM_UNSIGNED != 0;
    let mut buf = Vec::with_capacity(1 << 16);
    for s in r.samples() {
        let s = s.context("FLAC decode failed")?;
        match bits {
            8 => buf.push(if unsigned {
                (s + 128) as u8
            } else {
                s as i8 as u8
            }),
            16 => {
                let b = (s as i16).to_le_bytes();
                if be {
                    buf.extend([b[1], b[0]])
                } else {
                    buf.extend(b)
                }
            }
            24 => {
                let b = s.to_le_bytes();
                if be {
                    buf.extend([b[2], b[1], b[0]])
                } else {
                    buf.extend(&b[..3])
                }
            }
            _ => bail!("unsupported PCM bit depth {bits}"),
        }
        if buf.len() >= 1 << 16 {
            out.write_all(&buf)?;
            buf.clear();
        }
    }
    out.write_all(&buf)?;
    Ok(())
}
