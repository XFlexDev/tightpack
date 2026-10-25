# tightpack

An open archive format that makes already-compressed files smaller without changing a single bit.

Most of what people archive (photos, ZIPs, Office documents, APKs, PDFs, PNGs, WAVs) is already compressed, so zip, rar and 7z barely touch it. tightpack takes each file apart, recompresses the insides with better methods, and rebuilds the exact original bytes on extract. Every file is checked against a BLAKE3 hash.

## Results

87 MB mixed folder: JPEG photos, a ZIP of photos, a tar.gz of source code, a gzip file, a PDF, WAV and AIFF audio, MP3, MP4, PNGs and text.

| tool | size | of original | pack time |
|---|---:|---:|---:|
| zip -9 | 82.7 MB | 95.4% | 2.4s |
| tar + zstd -19 --long | 75.7 MB | 87.4% | 31.2s |
| tar + xz -9e | 73.9 MB | 85.3% | 42.4s |
| 7z -mx9 | 72.7 MB | 83.9% | 11.9s |
| rar -m5 solid | 72.6 MB | 83.8% | 4.5s |
| **tightpack** | **45.4 MB** | **52.4%** | 43.8s |
| **tightpack --max** | **44.6 MB** | **51.4%** | 84.9s |

Run `bench/run.sh <folder>` to reproduce it on your own files.

## What it does per file type

| input | method | typical saving vs original |
|---|---|---|
| JPEG | Lepton recoding of the DCT coefficients | 20-23% |
| ZIP, DOCX, XLSX, PPTX, JAR, APK, EPUB | inflate every entry, recompress solid, rebuild deflate exactly with preflate | 20-60% |
| gzip, .tar.gz | same, then the tar is split so its files dedupe and get transformed too | 20-40% |
| PNG | inflate IDAT, recompress, rebuild exactly | 10-40% |
| PDF | inflate FlateDecode streams | 20-50% |
| WAV, AIFF | PCM to FLAC | 30-60% |
| text, code, binaries | solid zstd or xz, best per block | like 7z |
| MP3, MP4, MKV, WebM, other dense data | stored in a separate stream so it doesn't hurt the rest | about 0% |

Transforms nest: a JPEG inside a ZIP inside a tar.gz still gets Lepton. If a transform fails or the rebuild is not bit-exact, the original bytes are stored instead, so packing never loses data. Identical files are stored once.

## Install

```
cargo install --git https://github.com/XFlexDev/tightpack
```

## Usage

```
tightpack pack photos/ -o photos.tpak      # default level
tightpack pack --max backup/ -o backup.tpak
tightpack pack --fast big.iso
tightpack unpack photos.tpak -C out/
tightpack list photos.tpak
tightpack test photos.tpak                 # rebuild everything and verify hashes
tightpack info photos.tpak                 # streams, blocks, codecs
```

Options: `--no-jpeg`, `--no-deflate`, `--no-audio` turn off individual transforms. Directories, symlinks, permissions and modification times are kept. Extraction refuses absolute paths and `..`.

## Library

```rust
use tightpack::{Level, PackOptions, Reader, Writer, archive::InputFile};

let opts = PackOptions { level: Level::Normal, ..Default::default() };
let mut w = Writer::new(std::fs::File::create("out.tpak")?, opts)?;
w.add_files(vec![InputFile {
    path: "hello.txt".into(),
    mode: 0o644,
    mtime: 0,
    data: b"hello".to_vec(),
}])?;
w.finish()?;

let mut r = Reader::open(std::fs::File::open("out.tpak")?)?;
for i in 0..r.entries.len() {
    let mut out = Vec::new();
    r.extract(i, &mut out)?;
}
```

## Format

The format is fully specified in [FORMAT.md](FORMAT.md). It is a header, compressed blocks of three logical streams (compressible data, already-coded data, container metadata), an index and a fixed trailer. Each file is described by a small recipe tree that says how to rebuild it.

## Limitations

- Video and MP3 are stored, not recompressed. Lossless gains there are small and not implemented yet.
- Small or unusually encoded deflate streams (some tiny PNGs) can't be modelled by preflate and are stored as-is.
- Files over 1 GiB are streamed without transforms.
- Packing is slower than 7z. Extraction is a few seconds per 100 MB.
- Version 1 of the format. It may still change before 1.0.

## License

MIT or Apache-2.0, at your option.
