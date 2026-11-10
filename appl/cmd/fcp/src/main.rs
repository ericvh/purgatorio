//! fcp [-r] [-R nr] [-W nw] src target, fcp [-r] [-R nr] [-W nw] src ...
//! directory: copy files, as `cp` does, keeping each source's permission
//! bits. It is meant for slow connections: Inferno's fcp keeps `nr` reads
//! and `nw` writes (8 each by default) outstanding at once. Here one prog
//! reads up to `nr` blocks of the file's I/O unit ahead and then writes
//! them, `nw` blocks per write; both must be at least 1. `-r` copies
//! directories: into `target/name` when `target` is a directory, else to
//! `target`. Errors with one file are reported and the rest are copied;
//! the exit status says whether any failed.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error, Qid};

const USAGE: &str = "\tfcp [-r] [-R nproc] [-W nproc] src target\n\tfcp [-r] [-R nproc] [-W nproc] src ... directory";

/// The block size when a file has no I/O unit.
const BLOCK: usize = 8192;

struct Fcp {
    recursive: bool,
    nr: usize,
    nw: usize,
    errors: usize,
}

fn base(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

fn same(a: &Dir, b: &Dir) -> bool {
    a.qid == b.qid && a.dtype == b.dtype && a.dev == b.dev
}

impl Fcp {
    fn warn(&mut self, msg: String) {
        eprintln!("fcp: {msg}");
        self.errors += 1;
    }

    fn file(&mut self, src: &str, dst: &str, d: &Dir) {
        if let Ok(t) = purgatorio::stat(dst)
            && same(d, &t)
        {
            return self.warn(format!("{src} and {dst} are the same file"));
        }
        let mut from = match purgatorio::open(src, mode::OREAD) {
            Ok(f) => File::from(f),
            Err(e) => return self.warn(format!("cannot open {src}: {e}")),
        };
        let mut to = match purgatorio::create(dst, mode::OWRITE, d.mode & 0o777) {
            Ok(f) => File::from(f),
            Err(e) => return self.warn(format!("cannot create {dst}: {e}")),
        };
        let unit = |f: &File| match purgatorio::iounit(f.as_raw_fd()) {
            Ok(0) | Err(_) => BLOCK,
            Ok(n) => n,
        };
        let (rblk, wblk) = (unit(&from), unit(&to));
        let mut buf = vec![0u8; rblk * self.nr];
        loop {
            // Fill the buffer, one block per read.
            let mut n = 0;
            while n < buf.len() {
                let end = (n + rblk).min(buf.len());
                match from.read(&mut buf[n..end]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => {
                        return self.warn(format!("error reading {src}: {}", Error::from(e)));
                    }
                }
            }
            if n == 0 {
                return;
            }
            for chunk in buf[..n].chunks(wblk * self.nw) {
                if let Err(e) = to.write_all(chunk) {
                    return self.warn(format!("error writing {dst}: {}", Error::from(e)));
                }
            }
            if n < buf.len() {
                return;
            }
        }
    }

    /// Copies `src` (stat `d`) to `dst`. `seen` holds the directories
    /// being copied, to catch a namespace that loops.
    fn copy(&mut self, src: &str, dst: &str, d: &Dir, seen: &mut Vec<Qid>) {
        if d.mode & DMDIR == 0 {
            return self.file(src, dst, d);
        }
        if !self.recursive {
            return self.warn(format!("{src} is a directory"));
        }
        if seen.contains(&d.qid) {
            return self.warn(format!("{src}: directory loop found"));
        }
        match purgatorio::stat(dst) {
            Ok(t) if t.mode & DMDIR == 0 => {
                return self.warn(format!("{dst}: destination not a directory"));
            }
            Ok(t) if same(d, &t) => {
                return self.warn(format!("cannot copy {src} into itself"));
            }
            Ok(_) => {}
            Err(_) => {
                if let Err(e) = purgatorio::create(dst, mode::OREAD, DMDIR | (d.mode & 0o777)) {
                    return self.warn(format!("cannot make directory {dst}: {e}"));
                }
            }
        }
        let ents = match std::fs::read_dir(src) {
            Ok(r) => r,
            Err(e) => return self.warn(format!("cannot open {src}: {}", Error::from(e))),
        };
        let mut names: Vec<String> = ents
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        seen.push(d.qid);
        for n in names {
            let (s, t) = (format!("{src}/{n}"), format!("{dst}/{n}"));
            match purgatorio::stat(&s) {
                Ok(sd) => self.copy(&s, &t, &sd, seen),
                Err(e) => self.warn(format!("can't stat {s}: {e}")),
            }
        }
        seen.pop();
    }
}

fn count(arg: &mut Arg) -> Result<usize, Error> {
    match arg.earg()?.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(arg.usage()),
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut f = Fcp {
        recursive: false,
        nr: 8,
        nw: 8,
        errors: 0,
    };
    while let Some(c) = arg.opt() {
        match c {
            'r' => f.recursive = true,
            'R' => f.nr = count(&mut arg)?,
            'W' => f.nw = count(&mut arg)?,
            _ => return Err(arg.usage()),
        }
    }
    let mut files = arg.argv();
    if files.len() < 2 {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let target = files.pop().unwrap_or_default();
    let tdir = purgatorio::stat(&target)
        .ok()
        .filter(|d| d.mode & DMDIR != 0);
    if files.len() > 1 && tdir.is_none() {
        return Err(Error(format!("{target}  not a directory")));
    }
    for src in &files {
        let d = match purgatorio::stat(src) {
            Ok(d) => d,
            Err(e) => {
                f.warn(format!("{src}: {e}"));
                continue;
            }
        };
        let dst = match &tdir {
            Some(t) if same(&d, t) => {
                f.warn(format!("cannot copy {src} into itself"));
                continue;
            }
            Some(_) => format!("{}/{}", target.trim_end_matches('/'), base(src)),
            None => target.clone(),
        };
        f.copy(src, &dst, &d, &mut Vec::new());
    }
    if f.errors > 0 {
        purgatorio::exits(&format!(
            "{} error{}",
            f.errors,
            if f.errors == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}
