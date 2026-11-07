//! Environment groups (Inferno's `Egrp`) and `#e`, which serves the current
//! prog's variables as files.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use purg_abi::{error, mode};
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};
use crate::prog::Procs;

struct Var {
    name: String,
    value: Vec<u8>,
    /// Unique across all Egrps, so Qids never collide.
    id: u64,
    vers: u32,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A set of environment variables, shared by progs unless `pctl` forks it.
#[derive(Default)]
pub struct Egrp {
    vars: Mutex<Vec<Var>>,
}

impl Egrp {
    pub fn new() -> Egrp {
        Egrp::default()
    }

    /// A copy (`FORKENV`).
    pub fn fork(&self) -> Egrp {
        let vars = self.vars.lock();
        Egrp {
            vars: Mutex::new(
                vars.iter()
                    .map(|v| Var {
                        name: v.name.clone(),
                        value: v.value.clone(),
                        id: next_id(),
                        vers: 0,
                    })
                    .collect(),
            ),
        }
    }

    pub fn get(&self, name: &str) -> Option<Vec<u8>> {
        self.vars
            .lock()
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.value.clone())
    }

    pub fn set(&self, name: &str, value: &[u8]) {
        let mut vars = self.vars.lock();
        match vars.iter_mut().find(|v| v.name == name) {
            Some(v) => {
                v.value = value.to_vec();
                v.vers = v.vers.wrapping_add(1);
            }
            None => vars.push(Var {
                name: String::from(name),
                value: value.to_vec(),
                id: next_id(),
                vers: 0,
            }),
        }
    }

    /// `name=value` strings, for WASI `environ_get`.
    pub fn environ(&self) -> Vec<String> {
        self.vars
            .lock()
            .iter()
            .map(|v| alloc::format!("{}={}", v.name, String::from_utf8_lossy(&v.value)))
            .collect()
    }
}

const QDIR: u64 = 0;

/// `#e`: the variables of the calling prog's Egrp.
pub struct DevEnv {
    procs: Arc<Procs>,
}

impl DevEnv {
    pub fn new(procs: Arc<Procs>) -> DevEnv {
        DevEnv { procs }
    }

    fn egrp(&self) -> Result<Arc<Egrp>> {
        Ok(self.procs.current()?.egrp())
    }

    fn dir(&self, v: &Var) -> Dir {
        Dir {
            dtype: 'e' as u16,
            qid: Qid {
                path: v.id,
                vers: v.vers,
                qtype: QTFILE,
            },
            mode: 0o666,
            length: v.value.len() as u64,
            name: v.name.clone(),
            uid: String::from(self.procs.user()),
            gid: String::from(self.procs.user()),
            ..Dir::default()
        }
    }

    fn with_var<T>(&self, c: &Chan, f: impl FnOnce(&mut Var) -> Result<T>) -> Result<T> {
        let egrp = self.egrp()?;
        let mut vars = egrp.vars.lock();
        let v = vars
            .iter_mut()
            .find(|v| v.id == c.qid.path)
            .ok_or(Error::new(error::ENONEXIST))?;
        f(v)
    }
}

impl Dev for DevEnv {
    fn dc(&self) -> char {
        'e'
    }

    fn name(&self) -> &'static str {
        "env"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QDIR,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QDIR {
            return Err(Error::new(error::ENOTDIR));
        }
        let egrp = self.egrp()?;
        let vars = egrp.vars.lock();
        let v = vars
            .iter()
            .find(|v| v.name == name)
            .ok_or(Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: Qid {
                path: v.id,
                vers: v.vers,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        if c.qid.path == QDIR {
            return Ok(Dir {
                dtype: 'e' as u16,
                qid: c.qid,
                mode: DMDIR | 0o775,
                name: String::from("/"),
                uid: String::from(self.procs.user()),
                gid: String::from(self.procs.user()),
                ..Dir::default()
            });
        }
        self.with_var(c, |v| Ok(self.dir(v)))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QDIR {
            return Ok(());
        }
        self.with_var(c, |v| {
            if omode & mode::OTRUNC != 0 {
                v.value.clear();
                v.vers = v.vers.wrapping_add(1);
            }
            Ok(())
        })
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.with_var(c, |v| Ok(read_str(&v.value, buf, offset)))
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        let egrp = self.egrp()?;
        let vars = egrp.vars.lock();
        Ok(vars.iter().map(|v| self.dir(v)).collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        self.with_var(c, |v| {
            let off = usize::try_from(offset).map_err(|_| Error::new(error::EBADARG))?;
            let end = off
                .checked_add(buf.len())
                .ok_or(Error::new(error::EBADARG))?;
            if v.value.len() < end {
                v.value.resize(end, 0);
            }
            v.value[off..end].copy_from_slice(buf);
            v.vers = v.vers.wrapping_add(1);
            Ok(buf.len())
        })
    }

    fn create(&self, dir: &Chan, name: &str, perm: u32) -> Result<Walked> {
        if dir.qid.path != QDIR || perm & DMDIR != 0 {
            return Err(Error::new(error::EPERM));
        }
        let egrp = self.egrp()?;
        egrp.set(name, b"");
        let vars = egrp.vars.lock();
        let v = vars.iter().find(|v| v.name == name).expect("just set");
        Ok(Walked {
            qid: Qid {
                path: v.id,
                vers: v.vers,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        let egrp = self.egrp()?;
        let mut vars = egrp.vars.lock();
        let i = vars
            .iter()
            .position(|v| v.id == c.qid.path)
            .ok_or(Error::new(error::ENONEXIST))?;
        vars.remove(i);
        Ok(())
    }
}
