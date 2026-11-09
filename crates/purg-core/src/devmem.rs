//! `#%`: a one-file view of how much memory the platform reports.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::Platform;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QROOT: u64 = 0;
const QMEM: u64 = 1;

pub struct DevMem {
    platform: Arc<dyn Platform>,
}

impl DevMem {
    pub fn new(platform: Arc<dyn Platform>) -> DevMem {
        DevMem { platform }
    }

    fn text(&self) -> String {
        let (used, total) = self.platform.memory();
        format!("{used} {total}\n")
    }
}

impl Dev for DevMem {
    fn dc(&self) -> char {
        '%'
    }

    fn name(&self) -> &'static str {
        "mem"
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
        if c.qid.path != QROOT || name != "mem" {
            return Err(Error::new(if c.qid.path == QROOT {
                error::ENONEXIST
            } else {
                error::ENOTDIR
            }));
        }
        Ok(Walked {
            qid: Qid {
                path: QMEM,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let (name, qtype, perm, length) = if c.qid.path == QROOT {
            ("#%", QTDIR, DMDIR | 0o555, 0)
        } else {
            ("mem", QTFILE, 0o444, self.text().len() as u64)
        };
        let user = self.platform.user();
        Ok(Dir {
            dtype: '%' as u16,
            qid: Qid {
                path: c.qid.path,
                vers: 0,
                qtype,
            },
            mode: perm,
            length,
            name: String::from(name),
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        })
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QMEM && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path != QMEM {
            return Err(Error::new(error::EISDIR));
        }
        Ok(read_str(self.text().as_bytes(), buf, offset))
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        let user = self.platform.user();
        Ok(alloc::vec![Dir {
            dtype: '%' as u16,
            qid: Qid {
                path: QMEM,
                vers: 0,
                qtype: QTFILE,
            },
            mode: 0o444,
            length: self.text().len() as u64,
            name: String::from("mem"),
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        }])
    }

    fn write(&self, _c: &Chan, _buf: &[u8], _offset: u64) -> Result<usize> {
        Err(Error::new(error::EPERM))
    }
}
