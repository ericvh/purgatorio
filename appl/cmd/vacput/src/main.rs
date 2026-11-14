//! vacput [-dv] [-i | -x] [-a addr] [-b blocksize] [-n name] [-u uid]
//! [-g gid] path ...: write the files and directories `path` to a Venti
//! server as a vac archive and print its score, `vac:` and 40 hex digits
//! (Inferno's vacput(1)).
//!
//! The archive's root holds each `path` under its own name. `-a` dials
//! `addr` instead of `$venti`; `-b` sets the block size (8192); `-n` the
//! name in the root block (`vac`); `-u` and `-g` the owner and group of
//! every file. `-i` or `-x` read a list of paths from standard input,
//! one per line, to include or exclude. `-v` prints each path as it is
//! written, `-d` debugging messages. Temporary (`DMTMP`) and
//! authentication files are skipped.
//!
//! Unlike Inferno's, the files of a directory go into the archive sorted
//! by name, so the same tree with the same times gives the same archive
//! whatever order the directory lists them in.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, Read};
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMAUTH, DMDIR, Error};
use venti::vac::{self, DMTMP, DirSink, Direntry, MetaSink, StreamWriter};
use venti::{DATATYPE, Entry, MAXLUMPSIZE, Session, Venti};

const USAGE: &str =
    "vacput [-dv] [-i | -x] [-a addr] [-b blocksize] [-n name] [-u uid] [-g gid] path ...";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    purgatorio::exits(&format!("fail:{msg}"));
}

struct Put {
    session: Session<File>,
    bsize: u32,
    dflag: bool,
    vflag: bool,
    uid: Option<String>,
    gid: Option<String>,
    pathgen: u64,
    include: Option<HashSet<String>>,
    exclude: Option<HashSet<String>>,
}

impl Put {
    fn say(&self, s: &str) {
        if self.dflag {
            eprintln!("{s}");
        }
    }

    fn usepath(&self, p: &str) -> bool {
        if let Some(t) = &self.include {
            return t.contains(p);
        }
        if let Some(t) = &self.exclude {
            return !t.contains(p);
        }
        true
    }

    fn writepath(&mut self, path: &str, s: &mut DirSink, ms: &mut MetaSink) {
        if !self.usepath(path) {
            return;
        }
        if self.vflag {
            println!("{path}");
        }
        let fd = purgatorio::open(path, mode::OREAD)
            .unwrap_or_else(|e| fail(&format!("opening {path}: {e}")));
        let mut dir = purgatorio::fstat(fd.as_raw_fd())
            .unwrap_or_else(|e| fail(&format!("fstat {path}: {e}")));
        if dir.mode & DMAUTH != 0 {
            eprintln!("{path}: is auth file, skipping");
            return;
        }
        if dir.mode & DMTMP != 0 {
            eprintln!("{path}: is temporary file, skipping");
            return;
        }
        let qid = self.pathgen;
        self.pathgen += 1;
        let (e, me) = if dir.mode & DMDIR != 0 {
            let mut ns = DirSink::new(self.bsize);
            let mut nms = MetaSink::new(self.bsize);
            let mut kids = Vec::new();
            loop {
                let d = purgatorio::dirread(fd.as_raw_fd())
                    .unwrap_or_else(|e| fail(&format!("dirread {path}: {e}")));
                if d.is_empty() {
                    break;
                }
                kids.extend(d.into_iter().map(|d| d.name));
            }
            drop(fd);
            kids.sort();
            for k in kids {
                let np = if path.ends_with('/') {
                    format!("{path}{k}")
                } else {
                    format!("{path}/{k}")
                };
                self.writepath(&np, &mut ns, &mut nms);
            }
            let e = ns
                .finish(&mut self.session)
                .unwrap_or_else(|e| fail(&format!("error flushing dirsink for {path}: {e}")));
            let me = nms
                .finish(&mut self.session)
                .unwrap_or_else(|e| fail(&format!("error flushing metasink for {path}: {e}")));
            (e, Some(me))
        } else {
            let e = self
                .writefile(path, File::from(fd))
                .unwrap_or_else(|e| fail(&e));
            (e, None)
        };
        match dir.name.as_str() {
            "/" => dir.name = String::from("root"),
            "." => dir.name = String::from("dot"),
            _ => {}
        }
        let mut de = Direntry::from_dir(&dir);
        de.qid = qid;
        if let Some(u) = &self.uid {
            de.uid = u.clone();
        }
        if let Some(g) = &self.gid {
            de.gid = g.clone();
        }
        de.entry = s.add(&e);
        de.mentry = me.map_or(0, |me| s.add(&me));
        self.say(&format!("{path}: entry {} mentry {}", de.entry, de.mentry));
        ms.add(de);
    }

    fn writefile(&mut self, path: &str, mut f: File) -> Result<Entry, String> {
        let bsize = self.bsize as usize;
        let mut w = StreamWriter::new(DATATYPE, vac::psize(self.bsize), self.bsize);
        let mut buf = vec![0u8; bsize];
        loop {
            let mut n = 0;
            while n < bsize {
                match f.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => return Err(format!("reading {path}: {}", Error::from(e))),
                }
            }
            w.write(&mut self.session, &buf[..n])
                .map_err(|e| format!("writing {path}: {e}"))?;
            if n != bsize {
                break;
            }
        }
        w.finish(&mut self.session)
            .map_err(|e| format!("error flushing filesink for {path}: {e}"))
    }
}

fn readpaths() -> HashSet<String> {
    let mut t = HashSet::new();
    for l in io::stdin().lock().lines() {
        match l {
            Ok(l) => {
                t.insert(l);
            }
            Err(e) => fail(&format!("reading paths: {}", Error::from(e))),
        }
    }
    t
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut addr = String::from(venti::DEFAULT_ADDR);
    let mut bsize = vac::DSIZE;
    let mut name = String::from("vac");
    let (mut dflag, mut vflag, mut iflag, mut xflag) = (false, false, false, false);
    let (mut uid, mut gid) = (None, None);
    while let Some(c) = arg.opt() {
        match c {
            'a' => addr = arg.earg()?,
            'b' => {
                bsize = arg
                    .earg()?
                    .parse()
                    .ok()
                    .filter(|b| (512..=MAXLUMPSIZE as u32).contains(b))
                    .ok_or_else(|| Error::from("bad block size"))?
            }
            'n' => name = arg.earg()?,
            'd' => dflag = true,
            'i' => iflag = true,
            'v' => vflag = true,
            'x' => xflag = true,
            'u' => uid = Some(arg.earg()?),
            'g' => gid = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let paths = arg.argv();
    if paths.is_empty() {
        return Err(usage);
    }
    if iflag && xflag {
        eprintln!("cannot have both -i and -x");
        return Err(usage);
    }
    let (mut include, mut exclude) = (None, None);
    if iflag {
        include = Some(readpaths());
    } else if xflag {
        exclude = Some(readpaths());
    }
    let session = venti::dial(&addr).unwrap_or_else(|e| fail(&e));
    let mut p = Put {
        session,
        bsize,
        dflag,
        vflag,
        uid,
        gid,
        pathgen: 0,
        include,
        exclude,
    };
    p.say("have connection");

    let mut topde = match purgatorio::stat(&paths[0]) {
        Ok(d) if paths.len() == 1 && d.mode & DMDIR != 0 => Direntry::from_dir(&d),
        _ => {
            let u = purgatorio::auth::user();
            let u = if u.is_empty() {
                String::from("nobody")
            } else {
                u
            };
            Direntry {
                uid: u.clone(),
                gid: u,
                mode: 0o777 | vac::MODEDIR,
                ..Direntry::default()
            }
        }
    };
    topde.elem = name.clone();
    topde.qid = p.pathgen;
    p.pathgen += 1;
    if let Some(u) = &p.uid {
        topde.uid = u.clone();
    }
    if let Some(g) = &p.gid {
        topde.gid = g.clone();
    }
    topde.ctime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32);

    let mut s = DirSink::new(bsize);
    let mut ms = MetaSink::new(bsize);
    for path in &paths {
        p.writepath(path, &mut s, &mut ms);
    }
    p.say("tree written");
    let e0 = s
        .finish(&mut p.session)
        .unwrap_or_else(|e| fail(&format!("writing top entry: {e}")));
    let e1 = ms
        .finish(&mut p.session)
        .unwrap_or_else(|e| fail(&format!("writing top meta entry: {e}")));
    topde.qidspace = Some((0, p.pathgen));
    let v: &mut dyn Venti = &mut p.session;
    let score = vac::write_root(v, &name, bsize, &e0, &e1, topde)
        .unwrap_or_else(|e| fail(&format!("writing root score: {e}")));
    println!("vac:{score}");
    p.session
        .sync()
        .unwrap_or_else(|e| fail(&format!("syncing server: {e}")));
    let _ = p.session.goodbye();
    Ok(())
}
