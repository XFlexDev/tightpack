//! Little helpers for the varint based index encoding described in FORMAT.md.

use anyhow::{Result, bail};

pub fn put_uv(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn put_iv(out: &mut Vec<u8>, v: i64) {
    put_uv(out, ((v << 1) ^ (v >> 63)) as u64);
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uv(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub struct Cur<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Cur<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn u8(&mut self) -> Result<u8> {
        let Some(&b) = self.buf.get(self.pos) else {
            bail!("index truncated")
        };
        self.pos += 1;
        Ok(b)
    }

    pub fn uv(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            if shift >= 64 || (shift == 63 && b > 1) {
                bail!("varint overflow");
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn iv(&mut self) -> Result<i64> {
        let u = self.uv()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            bail!("index truncated");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.uv()?;
        let n = usize::try_from(n)?;
        self.take(n)
    }

    pub fn len_u(&mut self) -> Result<usize> {
        Ok(usize::try_from(self.uv()?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip() {
        let mut b = Vec::new();
        let vals = [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for v in vals {
            put_uv(&mut b, v);
        }
        for v in [-1i64, 0, 5, -500, i64::MIN, i64::MAX] {
            put_iv(&mut b, v);
        }
        let mut c = Cur::new(&b);
        for v in vals {
            assert_eq!(c.uv().unwrap(), v);
        }
        for v in [-1i64, 0, 5, -500, i64::MIN, i64::MAX] {
            assert_eq!(c.iv().unwrap(), v);
        }
    }
}
