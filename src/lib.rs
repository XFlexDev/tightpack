//! Open archive format that losslessly recompresses already-compressed files.
//!
//! ```no_run
//! use tightpack::{Level, PackOptions, Reader, Writer, archive::InputFile};
//!
//! let opts = PackOptions { level: Level::Normal, ..Default::default() };
//! let mut w = Writer::new(std::fs::File::create("out.tpak")?, opts)?;
//! w.add_files(vec![InputFile {
//!     path: "hello.txt".into(),
//!     mode: 0o644,
//!     mtime: 0,
//!     data: b"hello".to_vec(),
//! }])?;
//! w.finish()?;
//!
//! let mut r = Reader::open(std::fs::File::open("out.tpak")?)?;
//! for i in 0..r.entries.len() {
//!     let mut out = Vec::new();
//!     r.extract(i, &mut out)?;
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

//! tightpack: an open archive format that losslessly recompresses already
//! compressed data (JPEG, deflate in ZIP/gzip/PNG/PDF, PCM audio) and then
//! packs everything into solid zstd/xz blocks. See FORMAT.md for the spec.

pub mod archive;
pub mod codec;
pub mod filters;
pub mod node;
pub mod transform;
pub mod wire;

pub use archive::{Entry, EntryKind, Level, PackOptions, Reader, Writer};
