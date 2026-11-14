//! disk/mkfs [-aprvxS] [-d dest] [-n name] [-s source] [-u users] [-z n]
//! [-G group] [-U user] proto ...: copy the files the prototypes select
//! from `source` (default `/`) to the tree at `dest` (default `/n/kfs`),
//! when they are out of date there; or with `-a` write them all to
//! standard output as an archive for disk/mkext (with `-x`, just a list of
//! names, times and sizes).
//!
//! An archive is a header line per file, `name mode uid gid mtime length`
//! (mode in octal), followed by the file's bytes, and a last line
//! `end of archive`. `-n name` sends kfscmd(8) commands to
//! `/chan/kfs.name.cmd`; `-z` is accepted, and blocks of zeros are copied.

use std::io::{Read, Write};

use proto::{File, Level, Namespace, quote};
use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};

const USAGE: &str = "disk/mkfs [-aprvxS] [-d root] [-n kfscmdname] [-s src-fs] [-u userfile] [-z n] [-G group] [-U user] proto ...";

#[derive(Default)]
struct Mkfs {
    archive: bool,
    list: bool,
    modes: bool,
    all: bool,
    verbose: bool,
    setuid: bool,
    dest: String,
    kfs: Option<String>,
    user: Option<String>,
    group: Option<String>,
    out: Vec<u8>,
    /// The prototype and line being processed, for messages.
    proto: String,
    line: usize,
}

impl Mkfs {
    fn warn(&self, msg: &str) {
        eprintln!("disk/mkfs: {}:{}: {msg}", self.proto, self.line);
    }

    fn flush(&mut self) -> Result<(), Error> {
        let mut o = std::io::stdout().lock();
        o.write_all(&self.out)?;
        o.flush()?;
        self.out.clear();
        Ok(())
    }

    /// The owner and mode `f` gets in the new tree.
    fn attrs(&self, f: &File) -> Dir {
        let mut d = f.dir.clone();
        // Files from a device that keeps no owners belong to inferno.
        if !matches!(d.dtype, 0x4d | 0x55) {
            d.uid = String::from("inferno");
            d.gid = String::from("inferno");
            let m = (d.mode >> 6) & 7;
            d.mode |= m | (m << 3);
        }
        if let Some(u) = &f.uid {
            d.uid = u.clone();
        }
        if let Some(g) = &f.gid {
            d.gid = g.clone();
        }
        if let Some(u) = &self.user {
            d.uid = u.clone();
        }
        if let Some(g) = &self.group {
            d.gid = g.clone();
        }
        d
    }

    fn file(&mut self, f: &File) -> Result<(), Error> {
        if self.list {
            let line = format!("{}\t{}\t{}\n", quote(&f.new), f.dir.mtime, f.dir.length);
            self.out.extend_from_slice(line.as_bytes());
            return self.flush();
        }
        let d = self.attrs(f);
        let isdir = d.mode & DMDIR != 0;
        if self.archive {
            let len = if isdir { 0 } else { d.length };
            let hdr = format!(
                "{} {:o} {} {} {} {len}\n",
                quote(&f.new),
                d.mode,
                quote(&d.uid),
                quote(&d.gid),
                d.mtime
            );
            self.out.extend_from_slice(hdr.as_bytes());
            if !isdir {
                match std::fs::read(&f.old) {
                    Ok(mut data) => {
                        // The header promised `length` bytes.
                        data.resize(len as usize, 0);
                        self.out.extend_from_slice(&data);
                    }
                    Err(e) => {
                        self.warn(&format!("can't open {}: {}", quote(&f.old), Error::from(e)));
                        self.out.resize(self.out.len() + len as usize, 0);
                    }
                }
            }
            return self.flush();
        }
        let newfile = format!("{}{}", self.dest.trim_end_matches('/'), f.new);
        let uptodate = !self.all && purgatorio::stat(&newfile).is_ok_and(|t| t.mtime >= d.mtime);
        if !uptodate {
            if isdir {
                self.mkdir(&newfile, &d);
            } else {
                if self.verbose {
                    eprintln!("{}", quote(&f.new));
                }
                self.copy(&f.old, &newfile, &d);
            }
        } else if self.modes {
            self.setmodes(&newfile, &d);
        }
        Ok(())
    }

    fn setmodes(&self, name: &str, d: &Dir) {
        let mut nd = Dir::null();
        nd.mode = d.mode;
        nd.mtime = d.mtime;
        if self.setuid || self.group.is_some() {
            nd.gid = d.gid.clone();
        }
        if let Err(e) = purgatorio::wstat(name, &nd) {
            self.warn(&format!("can't set modes for {}: {e}", quote(name)));
        }
        if self.setuid || self.user.is_some() {
            let mut nd = Dir::null();
            nd.uid = d.uid.clone();
            let _ = purgatorio::wstat(name, &nd);
        }
    }

    fn mkdir(&self, name: &str, d: &Dir) {
        if purgatorio::stat(name).is_err()
            && let Err(e) = purgatorio::create(name, mode::OREAD, d.mode)
        {
            self.warn(&format!("can't create {}: {e}", quote(name)));
            return;
        }
        self.setmodes(name, d);
    }

    fn copy(&self, old: &str, new: &str, d: &Dir) {
        let mut data = Vec::new();
        if let Err(e) = std::fs::File::open(old).and_then(|mut f| f.read_to_end(&mut data)) {
            self.warn(&format!("can't open {}: {}", quote(old), Error::from(e)));
            return;
        }
        let res = purgatorio::create(new, mode::OWRITE, d.mode & !DMDIR).and_then(|fd| {
            std::fs::File::from(fd)
                .write_all(&data)
                .map_err(Error::from)
        });
        if let Err(e) = res {
            self.warn(&format!("can't create {}: {e}", quote(new)));
            return;
        }
        self.setmodes(new, d);
    }

    fn kfscmd(&self, cmd: &str) {
        let Some(name) = &self.kfs else { return };
        let path = format!("/chan/kfs.{name}.cmd");
        let res = purgatorio::open(&path, mode::ORDWR).and_then(|fd| {
            std::fs::File::from(fd)
                .write_all(cmd.as_bytes())
                .map_err(Error::from)
        });
        if let Err(e) = res {
            eprintln!("disk/mkfs: can't open {}: {e}", quote(&path));
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut m = Mkfs {
        dest: String::from("/n/kfs"),
        ..Mkfs::default()
    };
    let mut source = String::from("/");
    let mut users = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => m.archive = true,
            'x' => m.list = true,
            'p' => m.modes = true,
            'r' => m.all = true,
            'v' => m.verbose = true,
            'S' => m.setuid = true,
            'd' => m.dest = arg.earg()?,
            'n' => m.kfs = Some(arg.earg()?),
            's' => source = arg.earg()?,
            'u' => users = Some(arg.earg()?),
            'z' => {
                arg.earg()?
                    .parse::<u32>()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| arg.usage())?;
            }
            'G' => m.group = Some(arg.earg()?),
            'U' => m.user = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let protos = arg.argv();
    if protos.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    m.list &= m.archive;
    if !m.archive {
        m.kfscmd("allow");
        if let Some(u) = &users {
            let d = Dir::null();
            m.copy(
                u,
                &format!("{}/adm/users", m.dest.trim_end_matches('/')),
                &d,
            );
            m.kfscmd("user");
        }
    }
    let mut errs = 0;
    for p in &protos {
        eprintln!("processing {p}");
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "disk/mkfs: can't open {}: {}: skipping",
                    quote(p),
                    Error::from(e)
                );
                errs += 1;
                continue;
            }
        };
        m.proto = p.clone();
        let mut files = Vec::new();
        let mut failed = false;
        proto::read(
            &text,
            &source,
            &Namespace,
            &mut |level, line, msg| {
                eprintln!("disk/mkfs: {p}:{line}: {msg}");
                failed |= level == Level::Error;
            },
            &mut |f| files.push(f.clone()),
        );
        if failed {
            purgatorio::exits("fail:error");
        }
        for f in &files {
            m.file(f)?;
        }
    }
    eprintln!("file system made");
    m.kfscmd("disallow");
    m.kfscmd("sync");
    if errs > 0 {
        purgatorio::exits("fail:skipped protos");
    }
    if m.archive && !m.list {
        m.out.extend_from_slice(b"end of archive\n");
        m.flush()?;
    }
    Ok(())
}
