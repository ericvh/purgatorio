//! `#d`: a directory of the current prog's file descriptors. Opening `#d/n`
//! is a dup of fd `n`; `#d/nctl` names the file behind it.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};
use crate::prog::Procs;

const QROOT: u64 = 0;

fn fd_qid(fd: i32, ctl: bool) -> u64 {
    ((fd as u64 + 1) << 1) | u64::from(ctl)
}

fn split(path: u64) -> Option<(i32, bool)> {
    if path == QROOT {
        return None;
    }
    Some((((path >> 1) - 1) as i32, path & 1 == 1))
}

pub struct DevDup {
    procs: Arc<Procs>,
}

impl DevDup {
    pub fn new(procs: Arc<Procs>) -> DevDup {
        DevDup { procs }
    }

    fn underlying(&self, fd: i32) -> Result<Arc<Chan>> {
        self.procs.current()?.fgrp().get(fd)
    }
}

impl Dev for DevDup {
    fn dc(&self) -> char {
        'd'
    }

    fn name(&self) -> &'static str {
        "dup"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QROOT,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QROOT {
            return Err(Error::new(error::ENOTDIR));
        }
        let (num, ctl) = match name.strip_suffix("ctl") {
            Some(n) => (n, true),
            None => (name, false),
        };
        let fd: i32 = num.parse().map_err(|_| Error::new(error::ENONEXIST))?;
        self.underlying(fd)?;
        Ok(Walked {
            qid: Qid {
                path: fd_qid(fd, ctl),
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let user = String::from(self.procs.user());
        if c.qid.path == QROOT {
            return Ok(Dir {
                dtype: 'd' as u16,
                qid: c.qid,
                mode: DMDIR | 0o555,
                name: String::from("#d"),
                uid: user.clone(),
                gid: user,
                ..Dir::default()
            });
        }
        let (fd, ctl) = split(c.qid.path).ok_or(Error::new(error::ENONEXIST))?;
        let u = self.underlying(fd)?;
        let mut d = if ctl {
            Dir {
                name: format!("{fd}ctl"),
                mode: 0o444,
                qid: Qid {
                    path: c.qid.path,
                    vers: 0,
                    qtype: QTFILE,
                },
                ..Dir::default()
            }
        } else {
            let mut d = u.dev.stat(&u)?;
            d.name = format!("{fd}");
            d.qid.path = c.qid.path;
            d
        };
        d.dtype = 'd' as u16;
        if d.uid.is_empty() {
            d.uid = user.clone();
            d.gid = user;
        }
        Ok(d)
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QROOT {
            return Ok(());
        }
        let (fd, ctl) = split(c.qid.path).ok_or(Error::new(error::ENONEXIST))?;
        if ctl && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EPERM));
        }
        let u = self.underlying(fd)?;
        *c.state.lock() = Some(Box::new(u));
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let (fd, ctl) = split(c.qid.path).ok_or(Error::new(error::EISDIR))?;
        if ctl {
            let u = self.underlying(fd)?;
            let text = format!("{} {}\n", u.name, mode_name(u.mode));
            return Ok(read_str(text.as_bytes(), buf, offset));
        }
        let u = opened(c)?;
        u.dev.read(&u, buf, offset)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        if c.qid.path != QROOT {
            let (fd, ctl) = split(c.qid.path).ok_or(Error::new(error::ENOTDIR))?;
            if ctl {
                return Err(Error::new(error::ENOTDIR));
            }
            let u = self.underlying(fd)?;
            return u.dev.dirread(&u);
        }
        let proc = self.procs.current()?;
        let user = String::from(self.procs.user());
        let mut dirs = Vec::new();
        for fd in proc.fgrp().fds() {
            for ctl in [false, true] {
                dirs.push(Dir {
                    dtype: 'd' as u16,
                    qid: Qid {
                        path: fd_qid(fd, ctl),
                        vers: 0,
                        qtype: QTFILE,
                    },
                    mode: if ctl { 0o444 } else { 0o666 },
                    name: if ctl {
                        format!("{fd}ctl")
                    } else {
                        format!("{fd}")
                    },
                    uid: user.clone(),
                    gid: user.clone(),
                    ..Dir::default()
                });
            }
        }
        Ok(dirs)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let (fd, ctl) = split(c.qid.path).ok_or(Error::new(error::EISDIR))?;
        if ctl {
            return Err(Error::new(error::EPERM));
        }
        let _ = fd;
        let u = opened(c)?;
        u.dev.write(&u, buf, offset)
    }
}

/// The file an open `#d/n` is a dup of. The Chan's state lock is released
/// before the caller does I/O: a read that blocks must not hold up a write
/// on the same open file.
fn opened(c: &Chan) -> Result<Arc<Chan>> {
    c.state
        .lock()
        .as_ref()
        .and_then(|s| s.downcast_ref::<Arc<Chan>>())
        .cloned()
        .ok_or(Error::new(error::EBADUSEFD))
}

fn mode_name(m: u8) -> &'static str {
    match m & mode::ACCESS {
        mode::OREAD => "r",
        mode::OWRITE => "w",
        mode::ORDWR => "rw",
        mode::OEXEC => "x",
        _ => "?",
    }
}
