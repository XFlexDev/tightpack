use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use tightpack::archive::{EntryKind, InputFile, PackOptions, Reader, Writer, transform_stats};
use tightpack::codec::Level;
use tightpack::node::Node;
use tightpack::transform;

const LARGE_FILE: u64 = 1 << 30;
const BATCH_BYTES: u64 = 256 << 20;

#[derive(Parser)]
#[command(
    name = "tightpack",
    version,
    about = "Lossless archiver that recompresses JPEG, PNG, ZIP, gzip, PDF and WAV before packing everything solid."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive from files and folders
    #[command(visible_alias = "c")]
    Pack {
        /// Files or folders to add
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output archive (defaults to <input>.tpak for a single input)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Faster, larger output
        #[arg(long, conflicts_with = "max")]
        fast: bool,
        /// Slowest, smallest output
        #[arg(long)]
        max: bool,
        /// Store JPEGs as-is instead of recompressing them
        #[arg(long)]
        no_jpeg: bool,
        /// Do not unpack deflate streams (ZIP, PNG, gzip, PDF)
        #[arg(long)]
        no_deflate: bool,
        /// Do not convert PCM audio to FLAC frames
        #[arg(long)]
        no_audio: bool,
        /// Overwrite the output if it exists
        #[arg(short, long)]
        force: bool,
    },
    /// Extract an archive
    #[command(visible_alias = "x")]
    Unpack {
        archive: PathBuf,
        /// Destination folder
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
        /// Overwrite existing files
        #[arg(short, long)]
        force: bool,
    },
    /// List archive contents
    #[command(visible_alias = "l")]
    List { archive: PathBuf },
    /// Rebuild every file in memory and verify its hash
    #[command(visible_alias = "t")]
    Test { archive: PathBuf },
    /// Show block and transform statistics
    Info { archive: PathBuf },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("tightpack: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Pack {
            inputs,
            output,
            fast,
            max,
            no_jpeg,
            no_deflate,
            no_audio,
            force,
        } => {
            let level = if fast {
                Level::Fast
            } else if max {
                Level::Max
            } else {
                Level::Normal
            };
            let opts = PackOptions {
                level,
                transforms: transform::Options {
                    jpeg: !no_jpeg,
                    deflate: !no_deflate,
                    audio: !no_audio,
                    verify: true,
                },
            };
            let output = match output {
                Some(o) => o,
                None if inputs.len() == 1 => {
                    let name = inputs[0]
                        .canonicalize()?
                        .file_name()
                        .context("cannot name output, pass -o")?
                        .to_os_string();
                    let mut n = name;
                    n.push(".tpak");
                    PathBuf::from(n)
                }
                None => bail!("pass -o <archive> when packing several inputs"),
            };
            pack(&inputs, &output, opts, force)
        }
        Cmd::Unpack {
            archive,
            dir,
            force,
        } => unpack(&archive, &dir, force),
        Cmd::List { archive } => list(&archive),
        Cmd::Test { archive } => test(&archive),
        Cmd::Info { archive } => info(&archive),
    }
}

struct Found {
    abs: PathBuf,
    name: String,
    kind: EntryKind,
    size: u64,
    mode: u32,
    mtime: i64,
}

fn archive_name(rel: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(
                s.to_str()
                    .with_context(|| format!("{} is not valid UTF-8", rel.display()))?
                    .to_owned(),
            ),
            Component::CurDir => {}
            _ => bail!("unsupported path {}", rel.display()),
        }
    }
    Ok(parts.join("/"))
}

fn meta_mode(m: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if m.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }
}

fn collect(inputs: &[PathBuf], output: &Path) -> Result<Vec<Found>> {
    let out_abs = output.canonicalize().ok();
    let mut found = Vec::new();
    for input in inputs {
        let abs = input
            .canonicalize()
            .with_context(|| format!("cannot read {}", input.display()))?;
        let base = abs
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| abs.clone());
        for e in walkdir::WalkDir::new(&abs)
            .follow_links(false)
            .sort_by_file_name()
        {
            let e = e?;
            if Some(e.path()) == out_abs.as_deref() {
                continue;
            }
            let rel = e.path().strip_prefix(&base).unwrap_or(e.path());
            let name = archive_name(rel)?;
            if name.is_empty() {
                continue;
            }
            let m = e.path().symlink_metadata()?;
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            let ft = m.file_type();
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else if ft.is_file() {
                EntryKind::File
            } else {
                continue;
            };
            found.push(Found {
                abs: e.path().to_path_buf(),
                name,
                kind,
                size: m.len(),
                mode: meta_mode(&m),
                mtime,
            });
        }
    }
    Ok(found)
}

fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", units[u])
    }
}

fn ext_key(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn pack(inputs: &[PathBuf], output: &Path, opts: PackOptions, force: bool) -> Result<()> {
    if output.exists() && !force {
        bail!("{} already exists (use -f to overwrite)", output.display());
    }
    let start = Instant::now();
    let found = collect(inputs, output)?;
    let tmp = output.with_extension("tpak.partial");
    let file = File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
    let mut w = Writer::new(BufWriter::with_capacity(1 << 20, file), opts)?;
    let mut files = Vec::new();
    for f in found {
        match f.kind {
            EntryKind::Dir => w.add_dir(f.name, f.mode, f.mtime),
            EntryKind::Symlink => {
                let t = fs::read_link(&f.abs)?;
                w.add_symlink(
                    f.name,
                    t.to_str()
                        .context("symlink target is not UTF-8")?
                        .to_owned(),
                    f.mode,
                    f.mtime,
                );
            }
            EntryKind::File => files.push(f),
        }
    }
    // Similar files next to each other compress better in solid blocks.
    files.sort_by(|a, b| {
        ext_key(&a.name)
            .cmp(&ext_key(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    let total: u64 = files.iter().map(|f| f.size).sum();
    let count = files.len();
    let mut done = 0u64;
    let mut i = 0;
    while i < files.len() {
        if files[i].size >= LARGE_FILE {
            let f = &files[i];
            let r = BufReader::new(
                File::open(&f.abs).with_context(|| format!("cannot read {}", f.abs.display()))?,
            );
            w.add_large_file(f.name.clone(), f.mode, f.mtime, r)?;
            done += f.size;
            i += 1;
        } else {
            let mut j = i;
            let mut bytes = 0;
            while j < files.len()
                && files[j].size < LARGE_FILE
                && (j == i || bytes + files[j].size <= BATCH_BYTES)
            {
                bytes += files[j].size;
                j += 1;
            }
            let batch: Vec<InputFile> = files[i..j]
                .par_iter()
                .map(|f| {
                    let data = fs::read(&f.abs)
                        .with_context(|| format!("cannot read {}", f.abs.display()))?;
                    Ok(InputFile {
                        path: f.name.clone(),
                        mode: f.mode,
                        mtime: f.mtime,
                        data,
                    })
                })
                .collect::<Result<_>>()?;
            w.add_files(batch)?;
            done += bytes;
            i = j;
        }
        eprint!(
            "\r  {} / {} ({} of {} files)   ",
            human(done),
            human(total),
            i,
            count
        );
    }
    eprintln!();
    let stats = transform_stats(w.entries());
    let (out, size) = w.finish()?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    fs::rename(&tmp, output)?;
    let secs = start.elapsed().as_secs_f64();
    let pct = if total > 0 {
        size as f64 * 100.0 / total as f64
    } else {
        0.0
    };
    println!(
        "{} -> {} ({pct:.1}%) in {secs:.1}s",
        human(total),
        human(size)
    );
    println!(
        "recompressed: {} jpeg, {} deflate streams, {} png, {} audio, {} duplicates",
        stats.jpeg, stats.deflate, stats.png, stats.pcm, stats.dedup
    );
    println!("wrote {}", output.display());
    Ok(())
}

fn open(p: &Path) -> Result<Reader<BufReader<File>>> {
    let f = File::open(p).with_context(|| format!("cannot open {}", p.display()))?;
    Reader::open(BufReader::new(f))
}

fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    let mut p = root.to_path_buf();
    for part in name.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains(':') && cfg!(windows)
        {
            bail!("refusing unsafe path {name:?}");
        }
        p.push(part);
    }
    Ok(p)
}

fn unpack(archive: &Path, dir: &Path, force: bool) -> Result<()> {
    let start = Instant::now();
    let mut r = open(archive)?;
    fs::create_dir_all(dir)?;
    let mut links = Vec::new();
    let mut dirs = Vec::new();
    let mut bytes = 0u64;
    for i in 0..r.entries.len() {
        let e = r.entries[i].clone();
        let dest = safe_join(dir, &e.path)?;
        match e.kind {
            EntryKind::Dir => {
                fs::create_dir_all(&dest)?;
                dirs.push((dest, e.mode, e.mtime));
            }
            EntryKind::Symlink => links.push((dest, e.target)),
            EntryKind::File => {
                if let Some(p) = dest.parent() {
                    fs::create_dir_all(p)?;
                }
                if dest.symlink_metadata().is_ok() && !force {
                    bail!("{} exists (use -f to overwrite)", dest.display());
                }
                let tmp = dest.with_file_name(format!(
                    ".{}.tpak-tmp",
                    dest.file_name().unwrap().to_string_lossy()
                ));
                let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
                if let Err(err) = r.extract(i, &mut out) {
                    drop(out);
                    let _ = fs::remove_file(&tmp);
                    return Err(err);
                }
                out.flush()?;
                drop(out);
                fs::rename(&tmp, &dest)?;
                set_meta(&dest, e.mode, e.mtime);
                bytes += e.size;
            }
        }
    }
    for (dest, mode, mtime) in dirs.into_iter().rev() {
        set_meta(&dest, mode, mtime);
    }
    for (dest, target) in links {
        if dest.symlink_metadata().is_ok() {
            if !force {
                bail!("{} exists (use -f to overwrite)", dest.display());
            }
            fs::remove_file(&dest)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &dest)?;
        #[cfg(not(unix))]
        eprintln!("skipping symlink {} -> {target}", dest.display());
    }
    println!(
        "extracted {} in {:.1}s",
        human(bytes),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn set_meta(p: &Path, mode: u32, mtime: i64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
    }
    let _ = filetime::set_file_mtime(p, filetime::FileTime::from_unix_time(mtime, 0));
}

fn describe(n: &Node) -> String {
    let mut tags = Vec::new();
    n.walk(&mut |n| {
        let t = match n {
            Node::Lepton(_) => "jpeg",
            Node::Deflate { .. } => "deflate",
            Node::PngIdat { .. } => "png",
            Node::Pcm { .. } => "flac",
            Node::SameAs(_) => "dup",
            _ => return,
        };
        if !tags.contains(&t) {
            tags.push(t);
        }
    });
    tags.join("+")
}

fn list(archive: &Path) -> Result<()> {
    let r = open(archive)?;
    for e in &r.entries {
        match e.kind {
            EntryKind::Dir => println!("{:>12}  {:<12}  {}/", "", "dir", e.path),
            EntryKind::Symlink => {
                println!("{:>12}  {:<12}  {} -> {}", "", "link", e.path, e.target)
            }
            EntryKind::File => println!("{:>12}  {:<12}  {}", e.size, describe(&e.node), e.path),
        }
    }
    Ok(())
}

fn test(archive: &Path) -> Result<()> {
    let mut r = open(archive)?;
    let mut n = 0;
    for i in 0..r.entries.len() {
        if r.entries[i].kind == EntryKind::File {
            r.extract(i, &mut std::io::sink())?;
            n += 1;
        }
    }
    println!("ok: {n} files verified");
    Ok(())
}

fn info(archive: &Path) -> Result<()> {
    let r = open(archive)?;
    let total: u64 = r.entries.iter().map(|e| e.size).sum();
    println!("archive   {}", human(r.archive_len));
    println!("original  {} in {} entries", human(total), r.entries.len());
    let names = ["data", "coded", "meta"];
    let codecs = ["store", "zstd", "xz"];
    for (s, name) in names.iter().enumerate() {
        let bs: Vec<_> = r
            .blocks()
            .iter()
            .filter(|b| b.stream as usize == s)
            .collect();
        if bs.is_empty() {
            continue;
        }
        let raw: u64 = bs.iter().map(|b| b.raw_len).sum();
        let comp: u64 = bs.iter().map(|b| b.comp_len).sum();
        let mut used: Vec<&str> = bs
            .iter()
            .map(|b| codecs.get(b.codec as usize).copied().unwrap_or("?"))
            .collect();
        used.dedup();
        println!(
            "stream {name:<5} {} -> {} in {} blocks ({})",
            human(raw),
            human(comp),
            bs.len(),
            used.join(",")
        );
    }
    let s = transform_stats(&r.entries);
    println!(
        "recompressed: {} jpeg, {} deflate streams, {} png, {} audio, {} duplicates",
        s.jpeg, s.deflate, s.png, s.pcm, s.dedup
    );
    Ok(())
}
