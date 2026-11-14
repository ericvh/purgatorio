//! disk/mkext [-h] [-d destdir] [-f] [-T] [-u] [-v] [file ...]: unpack a
//! disk/mkfs archive from standard input under `destdir` (default `/`).
//! With files named, only they (and what is below them) are unpacked, and
//! missing directories on their paths are made. `-h` prints the headers
//! instead, `-T` restores modification times, `-u` also restores owners,
//! `-f` replaces files that cannot be created, and `-v` names each file.

use std::io::{BufRead, BufReader, Read, Write};

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};

struct Mkext {
    dest: String,
    force: bool,
    times: bool,
    owners: bool,
    verbose: bool,
}

fn warn(msg: &str) {
    eprintln!("disk/mkext: {msg}");
}

fn parent(name: &str) -> &str {
    match name.rfind('/') {
        Some(i) if i > 0 => &name[..i],
        _ => "/",
    }
}

/// Creates `name`, making its parent writable for the moment if need be.
fn create(name: &str, m: u8, perm: u32) -> Result<std::os::fd::OwnedFd, Error> {
    match purgatorio::create(name, m, perm) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            let p = parent(name);
            let Ok(pd) = purgatorio::stat(p) else {
                return Err(e);
            };
            let mut nd = Dir::null();
            nd.mode = pd.mode | 0o222;
            if purgatorio::wstat(p, &nd).is_err() {
                return Err(e);
            }
            let res = purgatorio::create(name, m, perm);
            nd.mode = pd.mode;
            let _ = purgatorio::wstat(p, &nd);
            res
        }
    }
}

/// Whether `name` is `prefix` or below it.
fn fileprefix(prefix: &str, name: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|r| r.is_empty() || r.starts_with('/'))
}

impl Mkext {
    fn mkdirs(&self, name: &str) {
        let mut base = self.dest.clone();
        let elems: Vec<&str> = name.split('/').filter(|e| !e.is_empty()).collect();
        for (i, e) in elems.iter().enumerate() {
            if !base.is_empty() {
                let _ = create(&base, mode::OREAD, DMDIR | 0o775);
            }
            if i + 1 == elems.len() {
                break;
            }
            base = format!("{}/{e}", base.trim_end_matches('/'));
        }
    }

    fn setmodes(&self, name: &str, perm: u32, mtime: u32, uid: &str, gid: &str) {
        let mut d = Dir::null();
        d.mode = perm;
        if self.times {
            d.mtime = mtime;
        }
        if self.owners {
            d.uid = String::from(uid);
            d.gid = String::from(gid);
        }
        if let Err(e) = purgatorio::wstat(name, &d) {
            warn(&format!("can't set modes for {name}: {e}"));
        }
        if self.owners
            && let Ok(d) = purgatorio::stat(name)
        {
            if d.uid != uid {
                warn(&format!("{name}: uid mismatch {uid} {}", d.uid));
            }
            if d.gid != gid {
                warn(&format!("{name}: gid mismatch {gid} {}", d.gid));
            }
        }
    }

    fn mkdir(&self, name: &str, perm: u32, mtime: u32, uid: &str, gid: &str) {
        if create(name, mode::OREAD, perm).is_err()
            && !purgatorio::stat(name).is_ok_and(|d| d.mode & DMDIR != 0)
        {
            warn(&format!(
                "can't make directory {name}: {}",
                purgatorio::errstr()
            ));
            return;
        }
        self.setmodes(name, perm, mtime, uid, gid);
    }

    fn extract(&self, name: &str, perm: u32, mtime: u32, uid: &str, gid: &str, data: &[u8]) {
        if self.verbose {
            println!("x {name} {} bytes", data.len());
        }
        let fd = match create(name, mode::OWRITE, perm) {
            Ok(fd) => fd,
            Err(e) => {
                let retry = self.force && purgatorio::remove(name).is_ok();
                match retry.then(|| create(name, mode::OWRITE, perm)) {
                    Some(Ok(fd)) => fd,
                    _ => {
                        warn(&format!("can't make file {name}: {e}"));
                        return;
                    }
                }
            }
        };
        if let Err(e) = std::fs::File::from(fd).write_all(data) {
            warn(&format!("error writing {name}: {}", Error::from(e)));
        }
        self.setmodes(name, perm, mtime, uid, gid);
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(
        args,
        "disk/mkext [-h] [-d destdir] [-T] [-u] [-v] [file ...]",
    );
    let mut m = Mkext {
        dest: String::new(),
        force: false,
        times: false,
        owners: false,
        verbose: false,
    };
    let mut headers = false;
    while let Some(c) = arg.opt() {
        match c {
            'd' => m.dest = arg.earg()?,
            'f' => m.force = true,
            'h' => headers = true,
            'u' => {
                m.owners = true;
                m.times = true;
            }
            't' | 'T' => m.times = true,
            'v' => m.verbose = true,
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut out = std::io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            eprintln!("premature end of archive");
            out.flush()?;
            purgatorio::exits("fail: eof");
        }
        if line == "end of archive\n" {
            eprintln!("done");
            out.flush()?;
            return Ok(());
        }
        let fields = purgatorio::unquote_fields(&line);
        let [name, perm, uid, gid, mtime, bytes] = fields.as_slice() else {
            warn("too few fields in file header");
            continue;
        };
        let perm = u32::from_str_radix(perm, 8).unwrap_or(0);
        let mtime: u32 = mtime.parse().unwrap_or(0);
        let bytes: u64 = bytes.parse().unwrap_or(0);
        let mut data = Vec::new();
        (&mut input).take(bytes).read_to_end(&mut data)?;
        if (data.len() as u64) < bytes {
            if headers || (!files.is_empty() && !files.iter().any(|f| fileprefix(f, name))) {
                warn("premature eof");
            } else {
                warn(&format!("premature eof reading {name}"));
            }
            out.flush()?;
            purgatorio::exits("fail: error");
        }
        if !files.is_empty() {
            if !files.iter().any(|f| fileprefix(f, name)) {
                continue;
            }
            if !headers {
                m.mkdirs(name);
            }
        }
        let full = format!("{}{name}", m.dest);
        if headers {
            writeln!(
                out,
                "{} {perm:o} {uid} {gid} {mtime} {bytes}",
                proto::quote(&full)
            )?;
            continue;
        }
        if perm & DMDIR != 0 {
            m.mkdir(&full, perm, mtime, uid, gid);
        } else {
            m.extract(&full, perm, mtime, uid, gid, &data);
        }
    }
}
