//! Block codecs. Each block is compressed with every candidate codec for its
//! stream and level, and the smallest result is kept.

use std::io::Read;

use anyhow::{Context, Result, bail, ensure};
use zstd::zstd_safe::{CParameter, DParameter};

use crate::node::Class;

pub const STORE: u8 = 0;
pub const ZSTD: u8 = 1;
pub const XZ: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Level {
    Fast,
    #[default]
    Normal,
    Max,
}

impl Level {
    pub fn block_size(self) -> usize {
        match self {
            Level::Fast => 32 << 20,
            Level::Normal => 64 << 20,
            Level::Max => 256 << 20,
        }
    }
}

fn threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

fn zstd(data: &[u8], level: i32, window_log: u32) -> Result<Vec<u8>> {
    let mut c = zstd::bulk::Compressor::new(level)?;
    c.set_parameter(CParameter::WindowLog(window_log))?;
    c.set_parameter(CParameter::EnableLongDistanceMatching(true))?;
    c.set_parameter(CParameter::NbWorkers(threads()))?;
    c.set_parameter(CParameter::ChecksumFlag(false))?;
    Ok(c.compress(data)?)
}

fn xz(data: &[u8], preset: u32) -> Result<Vec<u8>> {
    // Split into independent xz blocks so every core gets work.
    let t = threads();
    let block = (data.len() as u64 / u64::from(t)).max(24 << 20);
    let stream = liblzma::stream::MtStreamBuilder::new()
        .preset(preset)
        .threads(t)
        .block_size(block)
        .check(liblzma::stream::Check::None)
        .encoder()?;
    let mut out = Vec::with_capacity(data.len() / 3);
    liblzma::read::XzEncoder::new_stream(data, stream).read_to_end(&mut out)?;
    Ok(out)
}

fn window_for(len: usize) -> u32 {
    (usize::BITS - len.max(1 << 20).leading_zeros()).clamp(20, 30)
}

pub fn compress(class: Class, level: Level, data: &[u8]) -> Result<(u8, Vec<u8>)> {
    let w = window_for(data.len());
    let mut best: (u8, Vec<u8>) = (STORE, data.to_vec());
    let mut consider = |codec: u8, out: Result<Vec<u8>>| -> Result<()> {
        let out = out?;
        if out.len() < best.1.len() {
            best = (codec, out);
        }
        Ok(())
    };
    match (class, level) {
        (Class::Coded, Level::Fast) => consider(ZSTD, zstd(data, 3, w))?,
        (Class::Coded, Level::Normal) => consider(ZSTD, zstd(data, 9, w))?,
        (Class::Coded, Level::Max) => consider(ZSTD, zstd(data, 19, w))?,
        (_, Level::Fast) => consider(ZSTD, zstd(data, 12, w))?,
        (_, Level::Normal) => {
            let (z, x) = rayon::join(|| zstd(data, 19, w), || xz(data, 9));
            consider(ZSTD, z)?;
            consider(XZ, x)?;
        }
        (_, Level::Max) => {
            let (z, x) = rayon::join(
                || zstd(data, 22, w),
                || xz(data, 9 | liblzma::stream::PRESET_EXTREME),
            );
            consider(ZSTD, z)?;
            consider(XZ, x)?;
        }
    }
    // Dense data that barely shrinks is cheaper to keep stored.
    if best.0 != STORE && class == Class::Coded && best.1.len() as f64 > data.len() as f64 * 0.995 {
        best = (STORE, data.to_vec());
    }
    Ok(best)
}

pub fn decompress(codec: u8, data: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let out = match codec {
        STORE => data.to_vec(),
        ZSTD => {
            let mut d = zstd::bulk::Decompressor::new()?;
            d.set_parameter(DParameter::WindowLogMax(31))?;
            d.decompress(data, raw_len)?
        }
        XZ => {
            let mut out = Vec::with_capacity(raw_len);
            liblzma::read::XzDecoder::new_multi_decoder(data)
                .take(raw_len as u64 + 1)
                .read_to_end(&mut out)
                .context("xz block corrupt")?;
            out
        }
        c => bail!("unknown codec {c}"),
    };
    ensure!(out.len() == raw_len, "block decompressed to the wrong size");
    Ok(out)
}
