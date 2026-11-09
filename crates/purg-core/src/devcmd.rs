//! `#C`, the command device. Write a command line to `ctl` and read the
//! process's standard output from `data`, its standard error from `stderr`
//! and its exit status from `wait`. Platforms with no host shell answer
//! with `"not implemented"`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::net::Command;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::Platform;
use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QROOT: u64 = 0;
const QCLONE: u64 = 1;
/// Above the small fixed qids. The id occupies bits 8..40, so this bit must
/// sit above them.
const BASE: u64 = 1 << 40;

const FDIR: u8 = 0;
const FCTL: u8 = 1;
const FDATA: u8 = 2;
const FSTDERR: u8 = 3;
const FWAIT: u8 = 4;
const FSTATUS: u8 = 5;

fn qfile(id: u32, file: u8) -> u64 {
    BASE | (u64::from(id) << 8) | u64::from(file)
}

fn id_of(path: u64) -> u32 {
    ((path >> 8) & 0xffff_ffff) as u32
}

fn file_of(path: u64) -> u8 {
    (path & 0xff) as u8
}

struct Conv {
    id: u32,
    cmd: Mutex<Option<Arc<dyn Command>>>,
}

/// `#C`.
pub struct DevCmd {
    platform: Arc<dyn Platform>,
    convs: Mutex<Vec<Arc<Conv>>>,
    next: Mutex<u32>,
}

impl DevCmd {
    pub fn new(platform: Arc<dyn Platform>) -> DevCmd {
        DevCmd {
            platform,
            convs: Mutex::new(Vec::new()),
            next: Mutex::new(0),
        }
    }

    fn alloc(&self) -> Arc<Conv> {
        let mut n = self.next.lock();
        let id = *n;
        *n += 1;
        let c = Arc::new(Conv {
            id,
            cmd: Mutex::new(None),
        });
        self.convs.lock().push(Arc::clone(&c));
        c
    }

    fn find(&self, id: u32) -> Option<Arc<Conv>> {
        self.convs.lock().iter().find(|c| c.id == id).cloned()
    }

    fn qid(path: u64) -> Qid {
        let qtype = if path == QROOT || (path >= BASE && file_of(path) == FDIR) {
            QTDIR
        } else {
            QTFILE
        };
        Qid {
            path,
            vers: 0,
            qtype,
        }
    }

    fn dirent(&self, path: u64) -> Dir {
        let (name, _qtype, perm) = match path {
            QROOT => (String::from("#C"), QTDIR, DMDIR | 0o555),
            QCLONE => (String::from("clone"), QTFILE, 0o666),
            q if q >= BASE => match file_of(q) {
                FDIR => (id_of(q).to_string(), QTDIR, DMDIR | 0o555),
                FCTL => (String::from("ctl"), QTFILE, 0o666),
                FDATA => (String::from("data"), QTFILE, 0o666),
                FSTDERR => (String::from("stderr"), QTFILE, 0o444),
                FWAIT => (String::from("wait"), QTFILE, 0o444),
                FSTATUS => (String::from("status"), QTFILE, 0o444),
                _ => (String::from("?"), QTFILE, 0o444),
            },
            _ => (String::from("?"), QTFILE, 0o444),
        };
        let user = self.platform.user();
        Dir {
            dtype: u16::from(b'C'),
            qid: Self::qid(path),
            mode: perm,
            name,
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        }
    }
}

impl Dev for DevCmd {
    fn dc(&self) -> char {
        'C'
    }

    fn name(&self) -> &'static str {
        "cmd"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Self::qid(QROOT),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let next = match c.qid.path {
            QROOT if name == "clone" => QCLONE,
            QROOT => {
                let id: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                let conv = self.find(id).ok_or(Error::new(error::ENONEXIST))?;
                qfile(conv.id, FDIR)
            }
            q if q >= BASE && file_of(q) == FDIR => match name {
                "ctl" => qfile(id_of(q), FCTL),
                "data" => qfile(id_of(q), FDATA),
                "stderr" => qfile(id_of(q), FSTDERR),
                "wait" => qfile(id_of(q), FWAIT),
                "status" => qfile(id_of(q), FSTATUS),
                _ => return Err(Error::new(error::ENONEXIST)),
            },
            _ => return Err(Error::new(error::ENOTDIR)),
        };
        Ok(Walked {
            qid: Self::qid(next),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dirent(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.qtype == QTDIR && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EISDIR));
        }
        if c.qid.path == QCLONE {
            let conv = self.alloc();
            c.qid = Self::qid(qfile(conv.id, FCTL));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path < BASE {
            return Err(Error::new(error::EISDIR));
        }
        let conv = self
            .find(id_of(c.qid.path))
            .ok_or(Error::new(error::ENONEXIST))?;
        match file_of(c.qid.path) {
            FCTL => Ok(read_str(format!("{}\n", conv.id).as_bytes(), buf, offset)),
            FDATA => {
                let cmd = conv.cmd.lock().clone().ok_or(Error::new("not started"))?;
                cmd.read_stdout(buf).map_err(Error::from)
            }
            FSTDERR => {
                let cmd = conv.cmd.lock().clone().ok_or(Error::new("not started"))?;
                cmd.read_stderr(buf).map_err(Error::from)
            }
            FWAIT => {
                let cmd = conv.cmd.lock().clone().ok_or(Error::new("not started"))?;
                let status = cmd.wait().map_err(Error::from)?;
                let text = if status.is_empty() {
                    String::from("\n")
                } else {
                    format!("{status}\n")
                };
                Ok(read_str(text.as_bytes(), buf, offset))
            }
            FSTATUS => {
                let text = match conv.cmd.lock().as_ref() {
                    Some(cmd) => cmd.status(),
                    None => String::from("Init"),
                };
                Ok(read_str(format!("{text}\n").as_bytes(), buf, offset))
            }
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let mut entries = Vec::new();
        match c.qid.path {
            QROOT => {
                entries.push(self.dirent(QCLONE));
                for conv in self.convs.lock().iter() {
                    entries.push(self.dirent(qfile(conv.id, FDIR)));
                }
            }
            q if q >= BASE && file_of(q) == FDIR => {
                for f in [FCTL, FDATA, FSTDERR, FWAIT, FSTATUS] {
                    entries.push(self.dirent(qfile(id_of(q), f)));
                }
            }
            _ => return Err(Error::new(error::ENOTDIR)),
        }
        Ok(entries)
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if c.qid.path < BASE || file_of(c.qid.path) != FCTL {
            return Err(Error::new(error::EPERM));
        }
        let conv = self
            .find(id_of(c.qid.path))
            .ok_or(Error::new(error::ENONEXIST))?;
        let line = core::str::from_utf8(buf).unwrap_or("").trim();
        if line.is_empty() {
            return Err(Error::new(error::EBADARG));
        }
        if conv.cmd.lock().is_some() {
            return Err(Error::new("already started"));
        }
        let cmd = self.platform.command(line).map_err(Error::from)?;
        *conv.cmd.lock() = Some(cmd);
        Ok(buf.len())
    }
}
