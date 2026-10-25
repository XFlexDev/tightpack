# tightpack archive format, version 1

This document is the complete specification. Anyone may implement it. All
integers are little-endian unless noted; `uv` is an unsigned LEB128 varint,
`iv` is a zig-zag encoded signed varint, `bytes` is `uv length` followed by
that many bytes.

## Layout

```
header (16 bytes)
block 0 .. block N-1      compressed stream blocks, any order
index                     zstd frame
trailer (32 bytes)
```

### Header

| offset | size | value |
|---|---|---|
| 0 | 8 | magic `54 50 41 4B 0D 0A 1A 0A` (`TPAK\r\n\x1a\n`) |
| 8 | 2 | format version, `1` |
| 10 | 6 | reserved, zero |

### Trailer (last 32 bytes of the file)

| offset | size | value |
|---|---|---|
| 0 | 8 | file offset of the index |
| 8 | 8 | compressed index length |
| 16 | 8 | uncompressed index length |
| 24 | 4 | CRC-32 (IEEE) of the compressed index bytes |
| 28 | 4 | magic `TPKI` |

The index must end exactly where the trailer starts.

## Streams and blocks

An archive holds three logical byte streams:

| id | name | contents |
|---|---|---|
| 0 | data | compressible bytes: raw files, plain text of DEFLATE streams |
| 1 | coded | entropy coded bytes: Lepton, FLAC, MP3, video, other dense data |
| 2 | meta | container headers and DEFLATE corrections |

Each stream is the concatenation, in index order, of the uncompressed
contents of its blocks. A block is compressed with one codec:

| codec | meaning |
|---|---|
| 0 | stored |
| 1 | one Zstandard frame (RFC 8878). Window up to 2^31; decoders must allow it |
| 2 | one or more concatenated `.xz` streams |

## Index

```
uv   version (1)
uv   block count
     per block:
       u8   stream id
       u8   codec
       uv   file offset
       uv   compressed length
       uv   uncompressed length
       u32  CRC-32 of the uncompressed block
uv   entry count
     per entry:
       u8     kind: 0 file, 1 directory, 2 symlink
       bytes  path, UTF-8, `/` separated, relative, no `.` or `..` parts
       uv     unix mode bits
       iv     modification time, unix seconds
       file:    uv size, 32 byte BLAKE3 hash of the content, recipe
       symlink: bytes target
```

Readers must verify the size and BLAKE3 hash of every file they rebuild.

## Recipes

A recipe is a tree that produces the file's exact bytes. Each node starts
with a tag byte.

| tag | name | fields | output |
|---|---|---|---|
| 0 | ref | u8 stream, uv offset, uv length | that range of the logical stream |
| 1 | concat | uv n, n recipes | the outputs joined |
| 2 | deflate | recipe plain, recipe corrections | raw DEFLATE (RFC 1951) bitstream rebuilt from the plain text and preflate-rs corrections |
| 3 | lepton | recipe | the JPEG file decoded from a Lepton payload |
| 4 | pcm | u8 bits (8, 16, 24), u8 flags, recipe | interleaved integer PCM decoded from a FLAC stream |
| 5 | png_idat | uv n, n x uv size, recipe data | for each size: big-endian u32 size, `IDAT`, the next `size` bytes of data, big-endian CRC-32 of type and bytes |
| 6 | same_as | uv entry index | the content of an earlier file entry (top level only, must not chain) |
| 7 | empty | | nothing |

PCM flags: bit 0 big-endian samples, bit 1 unsigned 8-bit samples (offset 128).
Samples are written as two's complement with the given width.

The DEFLATE corrections are produced and consumed by
[preflate-rs](https://github.com/microsoft/preflate-rs) 0.7. Lepton payloads are
produced by [lepton_jpeg](https://github.com/microsoft/lepton_jpeg_rust) 0.5.
FLAC payloads are standard FLAC streams (with the `fLaC` signature and
STREAMINFO), decodable by any FLAC decoder.

## Writer behaviour (informative)

The reference writer detects JPEG, PNG, ZIP (and ZIP based formats such as
DOCX, XLSX, JAR, APK, EPUB), gzip, tar, WAV, AIFF and PDF, applies the
transforms above recursively (for example a JPEG stored inside a ZIP), rebuilds
every transformed file and keeps the original bytes whenever the rebuild does
not match exactly. Identical files are stored once. Each block is compressed
with every candidate codec for its level and the smallest result is kept.
