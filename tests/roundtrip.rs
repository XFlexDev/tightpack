use std::io::{Cursor, Write};

use tightpack::archive::{InputFile, PackOptions, Reader, Writer};
use tightpack::codec::Level;
use tightpack::node::Node;

fn text(n: usize) -> Vec<u8> {
    let words = [
        "alpha ", "beta ", "gamma ", "delta ", "lorem ", "ipsum ", "\n",
    ];
    let mut x = 12345u32;
    (0..n)
        .flat_map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            words[(x >> 16) as usize % words.len()].bytes()
        })
        .collect()
}

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 3];
    let mut z = zlib_raw(data);
    out.append(&mut z);
    out.extend(crc32fast::hash(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

fn zlib_raw(data: &[u8]) -> Vec<u8> {
    // A stored-block DEFLATE stream is valid and lets the test avoid a zlib dependency.
    let mut out = Vec::new();
    let chunks: Vec<&[u8]> = data.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        out.extend((c.len() as u16).to_le_bytes());
        out.extend((!(c.len() as u16)).to_le_bytes());
        out.extend_from_slice(c);
    }
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend(zlib_raw(data));
    out.extend(adler32(data).to_be_bytes());
    out
}

fn woff(table: &[u8]) -> Vec<u8> {
    // WOFF requires compLength < origLength, so the table needs a real
    // compressed zlib stream, not stored deflate blocks.
    let mut comp = Vec::new();
    {
        use flate2::{Compression, write::ZlibEncoder};
        let mut e = ZlibEncoder::new(&mut comp, Compression::best());
        e.write_all(table).unwrap();
        e.finish().unwrap();
    }
    let total = 44 + 20 + comp.len();
    let mut w = Vec::new();
    w.extend(b"wOFF");
    w.extend([0, 1, 0, 0]); // flavor
    w.extend((total as u32).to_be_bytes()); // length
    w.extend(1u16.to_be_bytes()); // numTables
    w.extend(0u16.to_be_bytes()); // reserved
    w.extend((table.len() as u32).to_be_bytes()); // totalSfntSize
    w.extend([0, 1, 0, 0]); // version
    w.extend([0; 20]); // meta + priv offsets/lengths
    w.extend(b"TEST");
    w.extend(64u32.to_be_bytes()); // table offset
    w.extend((comp.len() as u32).to_be_bytes());
    w.extend((table.len() as u32).to_be_bytes());
    w.extend(0xDEADBEEFu32.to_be_bytes());
    w.extend(comp);
    w
}

fn gif(pixels: &[u8], w: u16, h: u16) -> Vec<u8> {
    let mut g = Vec::new();
    g.extend(b"GIF89a");
    g.extend(w.to_le_bytes());
    g.extend(h.to_le_bytes());
    g.extend([0xF0, 0, 0]); // GCT flag, 2-color palette
    g.extend([0, 0, 0, 255, 255, 255]);
    g.push(0x2C);
    g.extend(0u16.to_le_bytes());
    g.extend(0u16.to_le_bytes());
    g.extend(w.to_le_bytes());
    g.extend(h.to_le_bytes());
    g.push(0); // no local palette
    g.push(8); // LZW min code size
    let lzw = tightpack::filters::lzw_encode(8, pixels);
    for c in lzw.chunks(255) {
        g.push(c.len() as u8);
        g.extend(c);
    }
    g.push(0);
    g.push(0x3B);
    g
}

fn elf() -> Vec<u8> {
    let mut e = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
    e.extend([0; 9]);
    e.extend(2u16.to_le_bytes()); // ET_EXEC
    e.extend(62u16.to_le_bytes()); // x86-64
    e.extend([0; 44]);
    // Fake code with call/jmp rel32 sites and filler.
    for i in 0..300u32 {
        e.push(0xE8);
        e.extend((i.wrapping_mul(7919)).to_le_bytes());
    }
    e
}

fn wav(seconds: usize) -> Vec<u8> {
    let rate = 44100u32;
    let n = rate as usize * seconds;
    let mut pcm = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let l = ((t * 440.0 * std::f64::consts::TAU).sin() * 9000.0) as i16;
        let r = ((t * 660.0 * std::f64::consts::TAU).sin() * 7000.0) as i16;
        pcm.extend(l.to_le_bytes());
        pcm.extend(r.to_le_bytes());
    }
    let mut w = Vec::new();
    w.extend(b"RIFF");
    w.extend((36 + pcm.len() as u32).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(rate.to_le_bytes());
    w.extend((rate * 4).to_le_bytes());
    w.extend(4u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend((pcm.len() as u32).to_le_bytes());
    w.extend(pcm);
    w.extend(b"LIST\x04\x00\x00\x00INFO");
    w
}

fn pack(files: &[(&str, Vec<u8>)], level: Level) -> Vec<u8> {
    let mut w = Writer::new(
        Cursor::new(Vec::new()),
        PackOptions {
            level,
            ..Default::default()
        },
    )
    .unwrap();
    w.add_dir("d".into(), 0o755, 0);
    w.add_symlink("d/link".into(), "../a.txt".into(), 0o777, 0);
    let batch = files
        .iter()
        .map(|(p, d)| InputFile {
            path: (*p).into(),
            mode: 0o644,
            mtime: 1_700_000_000,
            data: d.clone(),
        })
        .collect();
    w.add_files(batch).unwrap();
    w.add_large_file("big.bin".into(), 0o600, -5, Cursor::new(noise(3 << 20, 9)))
        .unwrap();
    let (c, len) = w.finish().unwrap();
    let v = c.into_inner();
    assert_eq!(v.len() as u64, len);
    v
}

fn check(files: &[(&str, Vec<u8>)], archive: &[u8]) -> Reader<Cursor<Vec<u8>>> {
    let mut r = Reader::open(Cursor::new(archive.to_vec())).unwrap();
    for (p, d) in files {
        let i = r.entries.iter().position(|e| e.path == *p).unwrap();
        let mut out = Vec::new();
        r.extract(i, &mut out).unwrap();
        assert!(out == *d, "{p} differs");
    }
    let i = r.entries.iter().position(|e| e.path == "big.bin").unwrap();
    let mut out = Vec::new();
    r.extract(i, &mut out).unwrap();
    assert_eq!(out, noise(3 << 20, 9));
    assert_eq!(
        r.entries
            .iter()
            .find(|e| e.path == "d/link")
            .unwrap()
            .target,
        "../a.txt"
    );
    r
}

fn sample() -> Vec<(&'static str, Vec<u8>)> {
    let t = text(200_000);
    vec![
        ("a.txt", t.clone()),
        ("copy.txt", t.clone()),
        ("empty", Vec::new()),
        ("tiny", b"hi".to_vec()),
        ("t.gz", gzip(&t)),
        ("song.wav", wav(2)),
        ("font.woff", woff(&text(50_000))),
        ("img.gif", gif(&noise(60_000, 7), 300, 200)),
        ("app.elf", elf()),
        ("blob.zlib", zlib(&text(50_000))),
        ("pack.zst", zstd::bulk::compress(&text(50_000), 3).unwrap()),
        ("noise.bin", noise(500_000, 1)),
        (
            "fake.jpg",
            [&[0xFF, 0xD8, 0xFF][..], &noise(5000, 3)].concat(),
        ),
        (
            "fake.png",
            [&b"\x89PNG\r\n\x1a\n"[..], &noise(5000, 4)].concat(),
        ),
        ("fake.zip", [&b"PK\x03\x04"[..], &noise(5000, 5)].concat()),
        (
            "fake.wav",
            [&b"RIFF\0\0\0\0WAVEfmt "[..], &noise(5000, 6)].concat(),
        ),
    ]
}

#[test]
fn roundtrip_all_levels() {
    let files = sample();
    for level in [Level::Fast, Level::Normal] {
        let a = pack(&files, level);
        let r = check(&files, &a);
        let dup = r.entries.iter().find(|e| e.path == "copy.txt").unwrap();
        assert!(matches!(dup.node, Node::SameAs(_)));
        let pcm = r.entries.iter().find(|e| e.path == "song.wav").unwrap();
        let mut has_pcm = false;
        pcm.node
            .walk(&mut |n| has_pcm |= matches!(n, Node::Pcm { .. }));
        assert!(has_pcm, "wav should be stored as FLAC frames");
        let kinds = |p: &str| {
            let e = r.entries.iter().find(|e| e.path == p).unwrap();
            let mut s = Vec::new();
            e.node.walk(&mut |n| {
                s.push(match n {
                    Node::Gif { .. } => "gif",
                    Node::Bcj { .. } => "bcj",
                    Node::ZstdFrame { .. } => "zstd",
                    Node::Deflate { .. } => "deflate",
                    _ => "",
                })
            });
            s
        };
        assert!(kinds("img.gif").contains(&"gif"));
        assert!(kinds("app.elf").contains(&"bcj"));
        assert!(kinds("pack.zst").contains(&"zstd"));
        assert!(kinds("font.woff").contains(&"deflate"));
        assert!(kinds("blob.zlib").contains(&"deflate"));
    }
}

#[test]
fn rejects_corruption() {
    let files = sample();
    let mut a = pack(&files, Level::Fast);
    assert!(Reader::open(Cursor::new(a[..a.len() - 3].to_vec())).is_err());
    let mid = a.len() / 3;
    a[mid] ^= 0x55;
    let mut r = match Reader::open(Cursor::new(a)) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut failed = false;
    for i in 0..r.entries.len() {
        if r.entries[i].kind == tightpack::EntryKind::File {
            failed |= r.extract(i, &mut std::io::sink()).is_err();
        }
    }
    assert!(failed, "a flipped byte must be detected");
}

#[test]
fn streams_into_any_writer() {
    let files = sample();
    let a = pack(&files, Level::Fast);
    let mut r = Reader::open(Cursor::new(a)).unwrap();
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let i = r.entries.iter().position(|e| e.path == "a.txt").unwrap();
    let mut c = Count(0);
    r.extract(i, &mut c).unwrap();
    assert_eq!(c.0 as u64, r.entries[i].size);
}
