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
