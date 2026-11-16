//! vacget [-dptv] [-a addr] [vac:]score: restore the vac archive `score`
//! from a Venti server into the current directory (Inferno's vacget(1)).
//!
//! `-a` dials `addr` instead of `$venti`. `-t` lists the paths and writes
//! nothing; `-v` lists them as they are written. `-p` tries to give the
//! files their archived owner, group, mode and modification time. `-d`
//! prints debugging messages.

use purgatorio::fd::AsRawFd;
use purgatorio::fs::File;
use purgatorio::{eprintln, println};
use std::io::Write;

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMAPPEND, DMDIR, DMEXCL, Dir, Error};
use venti::vac::{
    self, DMTMP, Direntry, MODEAPPEND, MODEDIR, MODEEXCL, MODEPERM, MODESNAPSHOT, Source, VacDir,
};
use venti::{Session, Venti};

const USAGE: &str = "vacget [-dtv] [-a addr] [tag:]score";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    purgatorio::exits(&format!("fail:{msg}"));
}

struct Get {
    session: Session<File>,
    dflag: bool,
    pflag: bool,
    tflag: bool,
    vflag: bool,
}

impl Get {
    fn say(&self, s: &str) {
        if self.dflag {
            eprintln!("{s}");
        }
    }

    /// Creates `path` as Inferno's vacget does: read-write for all, the
    /// directory, append, exclusive and temporary bits kept; with `-p`
    /// then the archived owner, group and mode.
    fn create(&self, path: &str, omode: u8, de: &Direntry) -> Result<File, Error> {
        let emode = de.emode();
        let mut perm = (DMDIR | DMAPPEND | DMEXCL | DMTMP) & emode;
        perm |= 0o666;
        if emode & DMDIR != 0 {
            perm |= 0o777;
        }
        let fd = purgatorio::create(path, omode, perm)?;
        if self.pflag {
            let mut d = Dir::null();
            d.uid = de.uid.clone();
            d.gid = de.gid.clone();
            d.mode = emode;
            if let Err(e) = purgatorio::fwstat(fd.as_raw_fd(), &d) {
                eprintln!("fwstat {path} for uid/gid/mode: {e}");
                d.uid.clear();
                d.gid.clear();
                let _ = purgatorio::fwstat(fd.as_raw_fd(), &d);
            }
        }
        Ok(File::from(fd))
    }

    fn walk(&mut self, path: &str, vd: &mut VacDir) {
        self.say(&format!("start of walk: {path}"));
        let list = vd
            .list(&mut self.session)
            .unwrap_or_else(|e| fail(&format!("reading direntry in {path}: {e}")));
        for de in list {
            let newpath = format!("{path}/{}", de.elem);
            let (e, me) = vd
                .open(&mut self.session, &de)
                .unwrap_or_else(|e| fail(&format!("reading entry for {newpath}: {e}")));
            let oflags = de.mode & !(MODEPERM | MODEAPPEND | MODEEXCL | MODEDIR | MODESNAPSHOT);
            if oflags != 0 {
                eprintln!("{newpath}: not all bits in mode can be set: 0x{oflags:x}");
            }
            if self.tflag || self.vflag {
                println!("{newpath}");
            }
            if let Some(me) = me {
                if !self.tflag {
                    // An error is ignored: the directory may be there
                    // already, and if it could not be made, writing the
                    // files in it fails later.
                    let _ = self.create(&newpath, mode::OREAD, &de);
                }
                self.walk(&newpath, &mut VacDir::new(e, me));
                continue;
            }
            if self.tflag {
                continue;
            }
            self.say("writing file");
            let mut f = self
                .create(&newpath, mode::OWRITE, &de)
                .unwrap_or_else(|e| fail(&format!("creating {newpath}: {e}")));
            let mut src = Source::new(e);
            let mut off = 0u64;
            loop {
                let b = src
                    .read_at(&mut self.session, off, 8192)
                    .unwrap_or_else(|e| fail(&format!("reading vac {newpath}: {e}")));
                if b.is_empty() {
                    break;
                }
                f.write_all(&b).unwrap_or_else(|e| {
                    fail(&format!("writing local {newpath}: {}", Error::from(e)))
                });
                off += b.len() as u64;
            }
            if self.pflag {
                let mut d = Dir::null();
                d.mtime = de.mtime;
                if let Err(e) = purgatorio::fwstat(f.as_raw_fd(), &d) {
                    eprintln!("fwstat {newpath} for mtime: {e}");
                }
            }
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut addr = String::from(venti::DEFAULT_ADDR);
    let (mut dflag, mut pflag, mut tflag, mut vflag) = (false, false, false, false);
    while let Some(c) = arg.opt() {
        match c {
            'a' => addr = arg.earg()?,
            'd' => dflag = true,
            'p' => pflag = true,
            't' => tflag = true,
            'v' => vflag = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let [score] = arg.argv().try_into().map_err(|_| usage)?;
    let score = venti::parse_tagged(&score, "vac").unwrap_or_else(|e| fail(&e));
    let session = venti::dial(&addr).unwrap_or_else(|e| fail(&e));
    let mut g = Get {
        session,
        dflag,
        pflag,
        tflag,
        vflag,
    };
    g.say("have handshake");
    let v: &mut dyn Venti = &mut g.session;
    let (mut vd, _, _) = vac::vdroot(v, &score).unwrap_or_else(|e| fail(&e));
    g.say("starting walk");
    g.walk(".", &mut vd);
    let _ = g.session.goodbye();
    Ok(())
}
