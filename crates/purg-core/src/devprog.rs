//! `#p`: one directory per prog. M2 serves only `wait`, which a parent
//! reads to collect its children's exit messages; the rest of Inferno's
//! `/prog` files arrive in M3.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};
use crate::prog::Procs;

const QROOT: u64 = 0;
const QDIR: u64 = 1;
const QWAIT: u64 = 2;

/// `(name, kind, perm)` of the files in each prog directory.
const FILES: &[(&str, u64, u32)] = &[("wait", QWAIT, 0o400)];

pub struct DevProg {
    procs: Arc<Procs>,
}

impl DevProg {
    pub fn new(procs: Arc<Procs>) -> DevProg {
        DevProg { procs }
    }

    fn qid(pid: u32, kind: u64) -> Qid {
        Qid {
            path: (u64::from(pid) << 4) | kind,
            vers: 0,
            qtype: if kind == QWAIT { QTFILE } else { QTDIR },
        }
    }

    fn dir(&self, pid: u32, kind: u64) -> Dir {
        let (name, mode) = match kind {
            QROOT => (String::from("#p"), DMDIR | 0o555),
            QDIR => (format!("{pid}"), DMDIR | 0o555),
            _ => {
                let f = FILES.iter().find(|f| f.1 == kind).expect("known kind");
                (String::from(f.0), f.2)
            }
        };
        Dir {
            dtype: 'p' as u16,
            qid: DevProg::qid(pid, kind),
            mode,
            name,
            uid: String::from(self.procs.user()),
            gid: String::from(self.procs.user()),
            ..Dir::default()
        }
    }
}

fn split(c: &Chan) -> (u32, u64) {
    ((c.qid.path >> 4) as u32, c.qid.path & 0xf)
}

impl Dev for DevProg {
    fn dc(&self) -> char {
        'p'
    }

    fn name(&self) -> &'static str {
        "prog"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: DevProg::qid(0, QROOT),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let (pid, kind) = split(c);
        let qid = match kind {
            QROOT => {
                let pid: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                self.procs.get(pid).ok_or(Error::new(error::ENONEXIST))?;
                DevProg::qid(pid, QDIR)
            }
            QDIR => {
                let f = FILES.iter().find(|f| f.0 == name);
                DevProg::qid(pid, f.ok_or(Error::new(error::ENONEXIST))?.1)
            }
            _ => return Err(Error::new(error::ENOTDIR)),
        };
        Ok(Walked { qid, aux: None })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let (pid, kind) = split(c);
        Ok(self.dir(pid, kind))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let (pid, kind) = split(c);
        if kind == QWAIT {
            if omode & mode::ACCESS != mode::OREAD {
                return Err(Error::new(error::EPERM));
            }
            self.procs.get(pid).ok_or(Error::new(error::ENONEXIST))?;
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], _offset: u64) -> Result<usize> {
        let (pid, kind) = split(c);
        if kind != QWAIT {
            return Err(Error::new(error::EISDIR));
        }
        let p = self.procs.get(pid).ok_or(Error::new(error::ENONEXIST))?;
        let msg = p.wait();
        let n = buf.len().min(msg.len());
        buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
        Ok(n)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let (pid, kind) = split(c);
        Ok(match kind {
            QROOT => self
                .procs
                .pids()
                .into_iter()
                .map(|p| self.dir(p, QDIR))
                .collect(),
            _ => FILES.iter().map(|f| self.dir(pid, f.1)).collect(),
        })
    }

    fn write(&self, _c: &Chan, _buf: &[u8], _offset: u64) -> Result<usize> {
        Err(Error::new(error::EPERM))
    }
}
