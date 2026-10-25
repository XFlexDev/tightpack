//! Byte filters used by transforms: an x86 call/jump address normalizer
//! (like xz's BCJ filter) and GIF's LZW codec. Both directions of each
//! filter live here so writers and readers share one implementation.

use std::collections::HashMap;
use std::io::Write;

/// Turn x86 relative call/jump targets into absolute addresses, which makes
/// equal targets produce equal byte patterns and compress far better.
/// Bijective: `decode` below always restores the exact input.
pub fn bcj_x86_encode(data: &mut [u8]) {
    bcj_x86(data, true);
}

/// Inverse of [`bcj_x86_encode`]: absolute addresses back to relative offsets.
pub fn bcj_x86_decode(data: &mut [u8]) {
    bcj_x86(data, false);
}

fn bcj_x86(data: &mut [u8], encode: bool) {
    let end = data.len().saturating_sub(5);
    let mut i = 0;
    while i <= end {
        if data[i] & 0xFE == 0xE8 {
            let pos = (i + 5) as u32;
            let v = u32::from_le_bytes(data[i + 1..i + 5].try_into().unwrap());
            let v = if encode {
                v.wrapping_add(pos)
            } else {
                v.wrapping_sub(pos)
            };
            data[i + 1..i + 5].copy_from_slice(&v.to_le_bytes());
            i += 5;
        } else {
            i += 1;
        }
    }
}

/// Detect files that hold x86/x86-64 machine code: PE (exe/dll/sys) and
/// little-endian ELF objects/executables.
pub fn is_x86_exec(data: &[u8]) -> bool {
    if data.starts_with(b"MZ") && data.len() > 0x40 {
        let pe = u32::from_le_bytes(data[0x3C..0x40].try_into().unwrap()) as usize;
        if let Some(h) = data.get(pe..pe + 6)
            && h[..4] == *b"PE\0\0"
        {
            let machine = u16::from_le_bytes(h[4..6].try_into().unwrap());
            return matches!(machine, 0x014C | 0x0200 | 0x8664);
        }
        return false;
    }
    if data.starts_with(b"\x7fELF") && data.len() > 20 && data[5] == 1 {
        let machine = u16::from_le_bytes(data[18..20].try_into().unwrap());
        return matches!(machine, 3 | 62);
    }
    false
}

/// Decode a GIF image payload (concatenated sub-block bytes, no sub-block
/// lengths) into raw index pixels. `min_code` is the LZW minimum code size
/// stored before the sub-blocks.
pub fn lzw_decode(min_code: u8, data: &[u8]) -> Option<Vec<u8>> {
    if !(2..=8).contains(&min_code) {
        return None;
    }
    let clear = 1u16 << min_code;
    let eoi = clear + 1;
    let mut prefix = vec![0u16; 4096];
    let mut suffix = vec![0u8; 4096];
    let mut next = eoi + 1;
    let mut width = u32::from(min_code) + 1;
    let mut out = Vec::new();
    let mut stack: Vec<u8> = Vec::with_capacity(4096);
    let mut prev: Option<u16> = None;
    let mut pos = 0usize;
    let mut acc = 0u64;
    let mut acc_bits = 0u32;
    loop {
        while acc_bits < width {
            acc |= u64::from(*data.get(pos)?) << acc_bits;
            pos += 1;
            acc_bits += 8;
        }
        let code = (acc & ((1 << width) - 1)) as u16;
        acc >>= width;
        acc_bits -= width;
        if code == clear {
            next = eoi + 1;
            width = u32::from(min_code) + 1;
            prev = None;
            continue;
        }
        if code == eoi {
            return Some(out);
        }
        if code > next || code >= 4096 {
            return None;
        }
        // code == next is the KwKwK case: the string is prev's string plus its
        // own first byte, and the dictionary entry is not populated yet.
        let kwkwk = code == next;
        let mut c = if kwkwk { prev? } else { code };
        // Emit the string for `c` and remember its first byte.
        stack.clear();
        let first;
        loop {
            if c < clear {
                first = c as u8;
                stack.push(c as u8);
                break;
            }
            if stack.len() >= 4096 {
                return None;
            }
            stack.push(suffix[c as usize]);
            c = prefix[c as usize];
        }
        while let Some(b) = stack.pop() {
            out.push(b);
        }
        if kwkwk {
            out.push(first);
        }
        if let Some(p) = prev {
            if next >= 4096 {
                return None;
            }
            prefix[next as usize] = p;
            suffix[next as usize] = first;
            next += 1;
            if next >= (1 << width) && width < 12 {
                width += 1;
            }
        }
        prev = Some(code);
    }
}

/// LZW encoder matching the GIF convention: clear code, growing code width,
/// greedy longest match, and a clear code emitted when the dictionary fills.
/// Output is deterministic; writers must verify it equals the source stream.
pub fn lzw_encode(min_code: u8, pixels: &[u8]) -> Vec<u8> {
    let clear = 1u16 << min_code;
    let eoi = clear + 1;
    let mut dict: HashMap<(u16, u8), u16> = HashMap::new();
    let mut next = eoi + 1;
    let mut width = u32::from(min_code) + 1;
    let mut out = BitWriter {
        buf: Vec::new(),
        acc: 0u64,
        nbits: 0,
    };
    out.put(clear, width);
    let mut it = pixels.iter();
    let Some(&first) = it.next() else {
        out.put(eoi, width);
        return out.finish();
    };
    let mut cur = u16::from(first);
    for &b in it {
        if let Some(&c) = dict.get(&(cur, b)) {
            cur = c;
            continue;
        }
        out.put(cur, width);
        dict.insert((cur, b), next);
        next += 1;
        // The decoder's table lags one entry behind the encoder's, so grow the
        // width only when `next` exceeds the current width's capacity.
        if next > (1 << width) && width < 12 {
            width += 1;
        }
        if next >= 4096 {
            out.put(clear, width);
            dict.clear();
            next = eoi + 1;
            width = u32::from(min_code) + 1;
        }
        cur = u16::from(b);
    }
    out.put(cur, width);
    out.put(eoi, width);
    out.finish()
}

struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    fn put(&mut self, code: u16, width: u32) {
        self.acc |= u64::from(code) << self.nbits;
        self.nbits += width;
        while self.nbits >= 8 {
            self.buf.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.buf.push(self.acc as u8);
        }
        self.buf
    }
}

/// Split an encoded LZW stream into GIF sub-blocks of at most 255 bytes.
pub fn gif_subblocks(lzw: &[u8], out: &mut dyn Write) -> std::io::Result<()> {
    for c in lzw.chunks(255) {
        out.write_all(&[c.len() as u8])?;
        out.write_all(c)?;
    }
    out.write_all(&[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcj_roundtrips() {
        let mut data: Vec<u8> = (0..1000u32).map(|i| (i * 2654435761) as u8).collect();
        data[10] = 0xE8;
        data[11..15].copy_from_slice(&0x11223344u32.to_le_bytes());
        let orig = data.clone();
        bcj_x86_encode(&mut data);
        bcj_x86_decode(&mut data);
        assert_eq!(data, orig);
    }

    #[test]
    fn lzw_roundtrips() {
        let pixels: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let enc = lzw_encode(8, &pixels);
        assert_eq!(lzw_decode(8, &enc).unwrap(), pixels);
    }
}
