//! Format detection and reversible transforms. Every transform here is
//! lossless; `analyze` always re-builds the result and falls back to storing
//! the original bytes if anything does not round-trip bit for bit.

use std::io::Cursor;

use crate::filters;
use crate::node::{Class, NoRefs, Node, PCM_BIG_ENDIAN, PCM_UNSIGNED};

const MAX_DEPTH: u32 = 4;
const MIN_TRANSFORM: usize = 256;
const MAX_PLAIN: usize = 512 << 20;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub jpeg: bool,
    pub deflate: bool,
    pub audio: bool,
    /// GIF image data decoded to pixels and re-encoded on unpack.
    pub gif: bool,
    /// BCJ filtering for x86/x86-64 executables (PE/ELF).
    pub binary: bool,
    /// Recompress whole zstd frames (e.g. .zst, .tar.zst) when they
    /// re-encode to the identical bytes.
    pub zstd: bool,
    /// Rebuild every transformed file and compare before accepting it.
    pub verify: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            jpeg: true,
            deflate: true,
            audio: true,
            gif: true,
            binary: true,
            zstd: true,
            verify: true,
        }
    }
}

/// Turn a whole file into a recipe whose leaves are in-memory bytes.
pub fn analyze(data: &[u8], opt: &Options) -> Node {
    let node = analyze_at(data, 0, opt);
    if matches!(node, Node::Bytes(..) | Node::Empty) || !opt.verify {
        return node;
    }
    match node.materialize(&mut NoRefs) {
        Ok(rebuilt) if rebuilt == data => node,
        _ => Node::raw(classify(data), data),
    }
}

fn analyze_at(data: &[u8], depth: u32, opt: &Options) -> Node {
    if data.len() < MIN_TRANSFORM || depth > MAX_DEPTH {
        return Node::raw(classify(data), data);
    }
    let attempt = if opt.jpeg && data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        jpeg(data)
    } else if opt.deflate && data.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(data)
    } else if opt.deflate && data.starts_with(b"PK\x03\x04") {
        zip(data, depth, opt)
    } else if opt.deflate && data.starts_with(&[0x1f, 0x8b, 0x08]) {
        gzip(data, depth, opt)
    } else if data.len() >= 512 && &data[257..262] == b"ustar" {
        tar(data, depth, opt)
    } else if opt.audio && data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        wav(data)
    } else if opt.audio && data.len() > 12 && &data[..4] == b"FORM" && &data[8..12] == b"AIFF" {
        aiff(data)
    } else if opt.deflate && data.starts_with(b"%PDF") {
        pdf(data, depth, opt)
    } else if opt.deflate && data.starts_with(b"wOFF") {
        woff(data, depth, opt)
    } else if opt.gif && data.starts_with(b"GIF8") {
        gif(data, depth, opt)
    } else if opt.zstd && data.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        zstd_frame(data, depth, opt)
    } else if opt.binary && filters::is_x86_exec(data) {
        bcj_exec(data)
    } else if opt.deflate && is_zlib(data) {
        zlib(data, depth, opt)
    } else {
        None
    };
    attempt.unwrap_or_else(|| Node::raw(classify(data), data))
}

/// Pick a stream for bytes we do not transform: dense data skips the slow codecs.
pub fn classify(data: &[u8]) -> Class {
    const DENSE_MAGIC: &[&[u8]] = &[
        b"Rar!\x1a\x07",
        b"7z\xbc\xaf\x27\x1c",
        b"\xfd7zXZ\x00",
        b"\x28\xb5\x2f\xfd",
        b"BZh",
        b"OggS",
        b"fLaC",
        b"ID3",
        b"\x1a\x45\xdf\xa3",
        b"wOF2",
        b"\xff\x0a",
    ];
    if DENSE_MAGIC.iter().any(|m| data.starts_with(m)) {
        return Class::Coded;
    }
    if data.len() > 12
        && (&data[4..8] == b"ftyp" || (&data[..4] == b"RIFF" && &data[8..12] == b"WEBP"))
    {
        return Class::Coded;
    }
    if data.len() < 4096 {
        return Class::Data;
    }
    let mut hist = [0u32; 256];
    let mut n = 0u32;
    let step = (data.len() / 4).max(1);
    for i in 0..4 {
        let start = i * step;
        for &b in &data[start..(start + 16384).min(data.len())] {
            hist[b as usize] += 1;
            n += 1;
        }
    }
    let n = f64::from(n);
    let h: f64 = hist
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum();
    if h > 7.97 { Class::Coded } else { Class::Data }
}

fn jpeg(data: &[u8]) -> Option<Node> {
    let mut out = Cursor::new(Vec::with_capacity(data.len()));
    let feats = lepton_jpeg::EnabledFeatures::compat_lepton_vector_write();
    if data.len() > feats.max_jpeg_file_size as usize {
        return None;
    }
    lepton_jpeg::encode_lepton(
        &mut Cursor::new(data),
        &mut out,
        &feats,
        &lepton_jpeg::DEFAULT_THREAD_POOL,
    )
    .ok()?;
    let lep = out.into_inner();
    (lep.len() < data.len()).then(|| Node::Lepton(Box::new(Node::raw(Class::Coded, &lep))))
}

/// Replace a raw DEFLATE stream at the start of `data` with plain text plus
/// corrections. Returns the node and the number of compressed bytes consumed.
fn deflate(data: &[u8], depth: u32, opt: &Options, recurse: bool) -> Option<(Node, usize)> {
    let cfg = preflate_rs::PreflateConfig {
        plain_text_limit: MAX_PLAIN,
        verify_compression: false,
        ..Default::default()
    };
    let (res, plain) = preflate_rs::preflate_whole_deflate_stream(data, &cfg).ok()?;
    let used = res.compressed_size;
    if used == 0 || used > data.len() || res.corrections.len() > used / 3 + 16 {
        return None;
    }
    let plain = plain.text();
    let inner = if recurse {
        analyze_at(plain, depth + 1, opt)
    } else {
        Node::raw(Class::Data, plain)
    };
    let node = Node::Deflate {
        plain: Box::new(inner),
        corrections: Box::new(Node::Bytes(Class::Meta, res.corrections)),
    };
    Some((node, used))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}
fn le16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}
fn le32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}
fn le64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

fn png(data: &[u8]) -> Option<Node> {
    let mut pos = 8usize;
    let mut first = None;
    let mut sizes = Vec::new();
    let mut payload = Vec::new();
    let mut end = 0usize;
    while pos + 12 <= data.len() {
        let len = be32(data, pos)? as usize;
        let ty = &data[pos + 4..pos + 8];
        let next = pos.checked_add(12)?.checked_add(len)?;
        if next > data.len() {
            return None;
        }
        if ty == b"IDAT" {
            match first {
                None => first = Some(pos),
                Some(_) if end != pos => return None,
                _ => {}
            }
            sizes.push(len as u32);
            payload.extend_from_slice(&data[pos + 8..pos + 8 + len]);
            end = next;
        }
        pos = next;
        if ty == b"IEND" {
            break;
        }
    }
    let first = first?;
    if payload.len() < 6
        || payload[0] & 0x0f != 8
        || payload[1] & 0x20 != 0
        || (u16::from(payload[0]) << 8 | u16::from(payload[1])) % 31 != 0
    {
        return None;
    }
    let (def, used) = deflate(&payload[2..], 0, &Options::default(), false)?;
    let zlib = Node::concat(vec![
        Node::raw(Class::Meta, &payload[..2]),
        def,
        Node::raw(Class::Meta, &payload[2 + used..]),
    ]);
    Some(Node::concat(vec![
        Node::raw(Class::Meta, &data[..first]),
        Node::PngIdat {
            sizes,
            data: Box::new(zlib),
        },
        Node::raw(Class::Meta, &data[end..]),
    ]))
}

struct ZipEntry {
    start: usize,
    size: usize,
    method: u16,
    encrypted: bool,
}

fn zip(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    let search_from = data.len().saturating_sub(65557);
    let eocd = (search_from..=data.len().checked_sub(22)?)
        .rev()
        .find(|&i| data[i..i + 4] == *b"PK\x05\x06")?;
    let mut count = u64::from(le16(data, eocd + 10)?);
    let mut cd_off = u64::from(le32(data, eocd + 16)?);
    if (count == 0xFFFF || cd_off == 0xFFFF_FFFF)
        && eocd >= 20
        && data[eocd - 20..eocd - 16] == *b"PK\x06\x07"
    {
        let z64 = usize::try_from(le64(data, eocd - 12)?).ok()?;
        if data.get(z64..z64 + 4)? != b"PK\x06\x06" {
            return None;
        }
        count = le64(data, z64 + 32)?;
        cd_off = le64(data, z64 + 48)?;
    }
    let mut p = usize::try_from(cd_off).ok()?;
    let mut entries = Vec::new();
    for _ in 0..count {
        if data.get(p..p + 4)? != b"PK\x01\x02" {
            return None;
        }
        let flags = le16(data, p + 8)?;
        let method = le16(data, p + 10)?;
        let mut csize = u64::from(le32(data, p + 20)?);
        let usize_ = u64::from(le32(data, p + 24)?);
        let nlen = le16(data, p + 28)? as usize;
        let xlen = le16(data, p + 30)? as usize;
        let clen = le16(data, p + 32)? as usize;
        let mut lho = u64::from(le32(data, p + 42)?);
        let mut x = p + 46 + nlen;
        let xend = x + xlen;
        while x + 4 <= xend {
            let id = le16(data, x)?;
            let sz = le16(data, x + 2)? as usize;
            if id == 1 {
                let mut q = x + 4;
                if usize_ == 0xFFFF_FFFF {
                    q += 8;
                }
                if csize == 0xFFFF_FFFF {
                    csize = le64(data, q)?;
                    q += 8;
                }
                if lho == 0xFFFF_FFFF {
                    lho = le64(data, q)?;
                }
            }
            x += 4 + sz;
        }
        p = xend + clen;
        let lho = usize::try_from(lho).ok()?;
        if data.get(lho..lho + 4) != Some(b"PK\x03\x04") {
            continue;
        }
        let start = lho + 30 + le16(data, lho + 26)? as usize + le16(data, lho + 28)? as usize;
        let size = usize::try_from(csize).ok()?;
        if start.checked_add(size)? <= data.len() {
            entries.push(ZipEntry {
                start,
                size,
                method,
                encrypted: flags & 1 != 0,
            });
        }
    }
    entries.sort_by_key(|e| e.start);
    let mut parts = Vec::new();
    let mut cursor = 0usize;
    let mut changed = false;
    for e in entries {
        if e.start < cursor || e.size == 0 {
            continue;
        }
        parts.push(Node::raw(Class::Meta, &data[cursor..e.start]));
        let body = &data[e.start..e.start + e.size];
        let node = if e.encrypted {
            Node::raw(Class::Coded, body)
        } else if e.method == 8 {
            match deflate(body, depth, opt, true) {
                Some((n, used)) => {
                    changed = true;
                    Node::concat(vec![n, Node::raw(Class::Meta, &body[used..])])
                }
                None => Node::raw(classify(body), body),
            }
        } else if e.method == 0 {
            let n = analyze_at(body, depth + 1, opt);
            changed |= !matches!(n, Node::Bytes(..));
            n
        } else {
            Node::raw(classify(body), body)
        };
        parts.push(node);
        cursor = e.start + e.size;
    }
    parts.push(Node::raw(Class::Meta, &data[cursor..]));
    changed.then(|| Node::concat(parts))
}

fn gzip(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    let mut parts = Vec::new();
    let mut pos = 0usize;
    while data.len() >= pos + 18 && data[pos..pos + 3] == [0x1f, 0x8b, 0x08] {
        let flg = data[pos + 3];
        let mut h = pos + 10;
        if flg & 4 != 0 {
            h += 2 + le16(data, h)? as usize;
        }
        for bit in [8u8, 16] {
            if flg & bit != 0 {
                h += data.get(h..)?.iter().position(|&b| b == 0)? + 1;
            }
        }
        if flg & 2 != 0 {
            h += 2;
        }
        if h >= data.len() {
            break;
        }
        let Some((n, used)) = deflate(&data[h..], depth, opt, true) else {
            break;
        };
        let tail = (h + used + 8).min(data.len());
        parts.push(Node::raw(Class::Meta, &data[pos..h]));
        parts.push(n);
        parts.push(Node::raw(Class::Meta, &data[h + used..tail]));
        pos = tail;
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(Node::raw(classify(&data[pos..]), &data[pos..]));
    Some(Node::concat(parts))
}

fn tar_size(h: &[u8]) -> Option<u64> {
    let f = &h[124..136];
    if f[0] & 0x80 != 0 {
        return Some(f[1..].iter().fold(0u64, |a, &b| (a << 8) | u64::from(b)));
    }
    let s = std::str::from_utf8(f)
        .ok()?
        .trim_matches(|c: char| c == '\0' || c == ' ');
    if s.is_empty() {
        Some(0)
    } else {
        u64::from_str_radix(s, 8).ok()
    }
}

fn tar(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    let mut parts = Vec::new();
    let mut pos = 0usize;
    let mut meta_start = 0usize;
    let mut changed = false;
    while pos + 512 <= data.len() {
        let h = &data[pos..pos + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let sum: u64 = h
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(b)
                }
            })
            .sum();
        let stored = std::str::from_utf8(&h[148..156])
            .ok()?
            .trim_matches(|c: char| c == '\0' || c == ' ');
        if u64::from_str_radix(stored, 8).ok()? != sum {
            return None;
        }
        let size = usize::try_from(tar_size(h)?).ok()?;
        let body = pos + 512;
        let end = body.checked_add(size)?;
        if end > data.len() {
            return None;
        }
        let regular = matches!(h[156], b'0' | 0 | b'7');
        if regular && size >= MIN_TRANSFORM {
            parts.push(Node::raw(Class::Meta, &data[meta_start..body]));
            let n = analyze_at(&data[body..end], depth + 1, opt);
            changed |= !matches!(n, Node::Bytes(Class::Data, _));
            parts.push(n);
            meta_start = end;
        }
        pos = body + size.div_ceil(512) * 512;
    }
    parts.push(Node::raw(Class::Meta, &data[meta_start..]));
    changed.then(|| Node::concat(parts))
}

fn flac_encode(samples: &[i32], channels: usize, bits: usize, rate: usize) -> Option<Vec<u8>> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    let cfg = flacenc::config::Encoder::default().into_verified().ok()?;
    let src = flacenc::source::MemSource::from_samples(samples, channels, bits, rate);
    let stream = flacenc::encode_with_fixed_block_size(&cfg, src, cfg.block_size).ok()?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).ok()?;
    Some(sink.into_inner())
}

fn pcm_node(
    raw: &[u8],
    channels: usize,
    bits: usize,
    rate: usize,
    be: bool,
    unsigned: bool,
) -> Option<(Node, usize)> {
    if !(1..=8).contains(&channels)
        || !matches!(bits, 8 | 16 | 24)
        || !(1..=655_350).contains(&rate)
    {
        return None;
    }
    let width = bits / 8;
    let frame = width * channels;
    let usable = raw.len() / frame * frame;
    if usable < 4096 {
        return None;
    }
    let samples: Vec<i32> = raw[..usable]
        .chunks_exact(width)
        .map(|s| match (bits, be) {
            (8, _) => {
                if unsigned {
                    i32::from(s[0]) - 128
                } else {
                    i32::from(s[0] as i8)
                }
            }
            (16, false) => i32::from(i16::from_le_bytes([s[0], s[1]])),
            (16, true) => i32::from(i16::from_be_bytes([s[0], s[1]])),
            (24, false) => i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8,
            _ => i32::from_be_bytes([s[0], s[1], s[2], 0]) >> 8,
        })
        .collect();
    let flac = flac_encode(&samples, channels, bits, rate)?;
    if flac.len() >= usable {
        return None;
    }
    let flags = if be { PCM_BIG_ENDIAN } else { 0 } | if unsigned { PCM_UNSIGNED } else { 0 };
    Some((
        Node::Pcm {
            bits: bits as u8,
            flags,
            flac: Box::new(Node::raw(Class::Coded, &flac)),
        },
        usable,
    ))
}

fn wav(data: &[u8]) -> Option<Node> {
    let mut pos = 12usize;
    let mut fmt = None;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = le32(data, pos + 4)? as usize;
        let body = pos + 8;
        if id == b"fmt " && len >= 16 {
            let tag = le16(data, body)?;
            let ext_pcm = tag == 0xFFFE && len >= 26 && le16(data, body + 24)? == 1;
            if tag == 1 || ext_pcm {
                fmt = Some((
                    le16(data, body + 2)? as usize,
                    le32(data, body + 4)? as usize,
                    le16(data, body + 14)? as usize,
                    le16(data, body + 12)? as usize,
                ));
            }
        } else if id == b"data" {
            let (ch, rate, bits, align) = fmt?;
            if align != ch * bits / 8 {
                return None;
            }
            let end = body.saturating_add(len).min(data.len());
            let (node, used) = pcm_node(&data[body..end], ch, bits, rate, false, bits == 8)?;
            return Some(Node::concat(vec![
                Node::raw(Class::Meta, &data[..body]),
                node,
                Node::raw(classify(&data[body + used..]), &data[body + used..]),
            ]));
        }
        pos = body.checked_add(len)?.checked_add(len & 1)?;
    }
    None
}

fn aiff(data: &[u8]) -> Option<Node> {
    let mut pos = 12usize;
    let mut comm = None;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = be32(data, pos + 4)? as usize;
        let body = pos + 8;
        if id == b"COMM" && len >= 18 {
            let ch = u16::from_be_bytes(data.get(body..body + 2)?.try_into().ok()?) as usize;
            let bits = u16::from_be_bytes(data.get(body + 6..body + 8)?.try_into().ok()?) as usize;
            let exp =
                u16::from_be_bytes(data.get(body + 8..body + 10)?.try_into().ok()?) as i32 - 16383;
            let mant = u64::from_be_bytes(data.get(body + 10..body + 18)?.try_into().ok()?);
            let rate = if (0..64).contains(&exp) {
                (mant >> (63 - exp)) as usize
            } else {
                0
            };
            comm = Some((ch, bits, rate));
        } else if id == b"SSND" && len >= 8 {
            let (ch, bits, rate) = comm?;
            let off = be32(data, body)? as usize;
            let start = body.checked_add(8)?.checked_add(off)?;
            let end = body.saturating_add(len).min(data.len());
            if start >= end {
                return None;
            }
            let (node, used) = pcm_node(&data[start..end], ch, bits, rate, true, false)?;
            return Some(Node::concat(vec![
                Node::raw(Class::Meta, &data[..start]),
                node,
                Node::raw(classify(&data[start + used..]), &data[start + used..]),
            ]));
        }
        pos = body.checked_add(len)?.checked_add(len & 1)?;
    }
    None
}

/// zlib-wrapped file: 2-byte header, raw DEFLATE, 4-byte adler32.
fn is_zlib(data: &[u8]) -> bool {
    let (a, b) = (data[0], data[1]);
    a & 0x0f == 8 && b & 0x20 == 0 && (u16::from(a) << 8 | u16::from(b)) % 31 == 0
}

fn zlib(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    let (def, used) = deflate(&data[2..], depth, opt, true)?;
    Some(Node::concat(vec![
        Node::raw(Class::Meta, &data[..2]),
        def,
        Node::raw(Class::Meta, &data[2 + used..]),
    ]))
}

/// WOFF (v1) font: a 44-byte header, a 20-byte-per-table directory, then
/// zlib-compressed table data. Compressed tables become deflate nodes.
fn woff(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    if data.len() < 64 || be32(data, 8)? as usize != data.len() {
        return None;
    }
    let num = usize::from(u16::from_be_bytes(data[12..14].try_into().ok()?));
    let dir_end = 44usize.checked_add(20 * num)?;
    if dir_end > data.len() {
        return None;
    }
    let mut parts = vec![Node::raw(Class::Meta, &data[..dir_end])];
    let mut cursor = dir_end;
    let mut changed = false;
    for i in 0..num {
        let d = 44 + 20 * i;
        let off = be32(data, d + 4)? as usize;
        let clen = be32(data, d + 8)? as usize;
        let olen = be32(data, d + 12)? as usize;
        let end = off.checked_add(clen)?;
        if off < cursor || end > data.len() {
            return None;
        }
        parts.push(Node::raw(Class::Meta, &data[cursor..off]));
        let body = &data[off..end];
        let node = if clen < olen && body.len() > 2 && is_zlib(body) {
            match deflate(&body[2..], depth, opt, true) {
                Some((n, used)) => {
                    changed = true;
                    Node::concat(vec![
                        Node::raw(Class::Meta, &body[..2]),
                        n,
                        Node::raw(Class::Meta, &body[2 + used..]),
                    ])
                }
                None => Node::raw(classify(body), body),
            }
        } else {
            Node::raw(classify(body), body)
        };
        parts.push(node);
        cursor = end;
    }
    parts.push(Node::raw(Class::Meta, &data[cursor..]));
    changed.then(|| Node::concat(parts))
}

/// Whole-file zstd frame: decompress it, recurse into the payload (so
/// .tar.zst gets the tar treatment), and keep it only if recompressing with
/// a candidate level reproduces the exact original bytes.
fn zstd_frame(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    let plain = zstd::stream::decode_all(Cursor::new(data)).ok()?;
    if plain.len() > MAX_PLAIN {
        return None;
    }
    for level in [3, 19, 22, 9, 1] {
        if zstd::bulk::compress(&plain, level).ok()?.as_slice() == data {
            let inner = analyze_at(&plain, depth + 1, opt);
            return Some(Node::ZstdFrame {
                level,
                plain: Box::new(inner),
            });
        }
    }
    None
}

/// x86 executables: normalize relative call/jump targets so identical
/// addresses compress together. The inverse filter restores exact bytes.
fn bcj_exec(data: &[u8]) -> Option<Node> {
    let mut filtered = data.to_vec();
    filters::bcj_x86_encode(&mut filtered);
    if filtered == data {
        return None;
    }
    Some(Node::Bcj {
        arch: 1,
        data: Box::new(Node::raw(Class::Data, &filtered)),
    })
}

fn skip_subblocks(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let n = usize::from(*data.get(pos)?);
        pos += 1;
        if n == 0 {
            return Some(pos);
        }
        pos = pos.checked_add(n)?;
        if pos > data.len() {
            return None;
        }
    }
}

/// GIF images: decode each image's LZW payload to index pixels. The pixels
/// go to the data stream; on unpack the deterministic LZW encoder rebuilds
/// the stream and the original sub-block framing is replayed.
fn gif(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    if !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        return None;
    }
    let mut pos = 6usize;
    let packed = *data.get(pos + 4)?;
    pos += 7;
    if packed & 0x80 != 0 {
        pos = pos.checked_add(3usize << ((packed & 7) + 1))?;
    }
    if pos > data.len() {
        return None;
    }
    let mut parts = vec![Node::raw(Class::Meta, &data[..pos])];
    let mut changed = false;
    loop {
        match *data.get(pos)? {
            // Leave pos on the trailer so the final meta part includes it.
            0x3B => break,
            0x21 => {
                let s = pos;
                pos += 2;
                pos = skip_subblocks(data, pos)?;
                parts.push(Node::raw(Class::Meta, &data[s..pos]));
            }
            0x2C => {
                let s = pos;
                let desc = *data.get(pos + 9)?;
                pos += 10;
                if desc & 0x80 != 0 {
                    pos = pos.checked_add(3usize << ((desc & 7) + 1))?;
                }
                parts.push(Node::raw(Class::Meta, &data[s..pos]));
                let min_code = *data.get(pos)?;
                pos += 1;
                let mut sizes = Vec::new();
                let mut payload = Vec::new();
                loop {
                    let n = usize::from(*data.get(pos)?);
                    pos += 1;
                    if n == 0 {
                        break;
                    }
                    if pos.checked_add(n)? > data.len() {
                        return None;
                    }
                    sizes.push(n as u32);
                    payload.extend_from_slice(&data[pos..pos + n]);
                    pos += n;
                }
                let pixels = filters::lzw_decode(min_code, &payload)?;
                parts.push(Node::Gif {
                    min_code_size: min_code,
                    sizes,
                    pixels: Box::new(analyze_at(&pixels, depth + 1, opt)),
                });
                changed = true;
            }
            _ => return None,
        }
    }
    parts.push(Node::raw(Class::Meta, &data[pos..]));
    changed.then(|| Node::concat(parts))
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn pdf(data: &[u8], depth: u32, opt: &Options) -> Option<Node> {
    let mut parts = Vec::new();
    let mut cursor = 0usize;
    let mut at = 0usize;
    let mut changed = false;
    while let Some(k) = find(data, b"stream", at) {
        at = k + 6;
        let mut s = at;
        if data.get(s) == Some(&b'\r') {
            s += 1;
        }
        if data.get(s) != Some(&b'\n') {
            continue;
        }
        s += 1;
        let (Some(&a), Some(&b)) = (data.get(s), data.get(s + 1)) else {
            break;
        };
        if a & 0x0f != 8 || b & 0x20 != 0 || (u16::from(a) << 8 | u16::from(b)) % 31 != 0 {
            continue;
        }
        let limit = find(data, b"endstream", s).unwrap_or(data.len());
        if limit < s + 2 + 64 {
            continue;
        }
        if let Some((n, used)) = deflate(&data[s + 2..limit], depth, opt, true) {
            parts.push(Node::raw(Class::Data, &data[cursor..s + 2]));
            parts.push(n);
            cursor = s + 2 + used;
            at = cursor;
            changed = true;
        }
    }
    parts.push(Node::raw(Class::Data, &data[cursor..]));
    changed.then(|| Node::concat(parts))
}
