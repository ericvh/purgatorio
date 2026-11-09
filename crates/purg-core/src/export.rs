//! `export`: serve a namespace over a file descriptor as a Styx server.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;
use purg_styx::server::{StyxFs, dispatch};
use purg_styx::{Dir, Msg, Qid};

use crate::chan::Chan;
use crate::error::{Error, Result as KResult};
use crate::names::cleanname;
use crate::pgrp::{Pgrp, absolute};
use crate::rw::{read_msg, write_full};
use crate::sysfile::{self, open};

struct Fid {
    path: String,
    chan: Chan,
}

/// Serves `root` (a path in `pgrp`) as 9P2000 on `fd` until the client hangs up.
pub fn serve(pgrp: Arc<Pgrp>, root: &str, fd: &Chan) -> KResult<()> {
    let mut fs = Export::new(pgrp, root)?;
    let mut msize = purg_styx::MAX_MSIZE;
    loop {
        let buf = match read_msg(fd, msize) {
            Ok(b) => b,
            Err(e) if e.as_str() == error::EHUNGUP || e.as_str() == error::EINTR => return Ok(()),
            Err(e) => return Err(e),
        };
        let msg = match Msg::decode(&buf, msize) {
            Ok(m) => m,
            Err(_) => return Err(Error::new(error::EIO)),
        };
        let reply = dispatch(&mut fs, &msg, &mut msize);
        write_full(fd, &reply)?;
    }
}

struct Export {
    pgrp: Arc<Pgrp>,
    root: String,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

impl Export {
    fn new(pgrp: Arc<Pgrp>, root: &str) -> KResult<Export> {
        let chan = pgrp.namec(root, "/")?;
        if !chan.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        Ok(Export {
            root: chan.name.clone(),
            pgrp,
            fids: BTreeMap::new(),
            iounit: 8192,
        })
    }

    fn step(&self, path: &str, name: &str) -> KResult<Chan> {
        let next = if name == ".." {
            if path == self.root || path == "/" {
                self.root.clone()
            } else {
                match path.rsplit_once('/') {
                    Some((p, _)) if !p.is_empty() => {
                        let p = String::from(p);
                        if self.root != "/" && !p.starts_with(&self.root) {
                            self.root.clone()
                        } else {
                            p
                        }
                    }
                    _ => self.root.clone(),
                }
            }
        } else {
            let mut s = String::from(path);
            if !s.ends_with('/') {
                s.push('/');
            }
            s.push_str(name);
            cleanname(&s)
        };
        self.pgrp.namec(&next, "/")
    }
}

impl StyxFs for Export {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(purg_styx::IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        let path = if aname.is_empty() {
            self.root.clone()
        } else {
            absolute(aname, &self.root)
        };
        let chan = self.pgrp.namec(&path, "/").map_err(|e| e.to_string())?;
        let qid = chan.qid;
        self.fids.insert(
            fid,
            Fid {
                path: chan.name.clone(),
                chan,
            },
        );
        Ok(qid)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let base = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let mut path = base.path.clone();
        let mut chan = base.chan.clone_unopened();
        if names.is_empty() {
            self.fids.insert(newfid, Fid { path, chan });
            return Ok(Vec::new());
        }
        let mut qids = Vec::new();
        for (i, name) in names.iter().enumerate() {
            match self.step(&path, name) {
                Ok(c) => {
                    qids.push(c.qid);
                    path = c.name.clone();
                    chan = c;
                }
                Err(e) => {
                    if i == 0 {
                        return Err(e.to_string());
                    }
                    return Ok(qids);
                }
            }
        }
        self.fids.insert(newfid, Fid { path, chan });
        Ok(qids)
    }

    fn open(&mut self, fid: u32, mode: u8) -> Result<(Qid, u32), String> {
        let f = self
            .fids
            .get_mut(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let opened = open(f.chan.clone_unopened(), mode).map_err(|e| e.to_string())?;
        let qid = opened.qid;
        f.chan = opened;
        Ok((qid, self.iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, mode: u8) -> Result<(Qid, u32), String> {
        let path = {
            let f = self
                .fids
                .get(&fid)
                .ok_or_else(|| String::from("unknown fid"))?;
            let mut p = f.path.clone();
            if !p.ends_with('/') {
                p.push('/');
            }
            p.push_str(name);
            p
        };
        let (dir, leaf) = self
            .pgrp
            .create_target(&path, "/")
            .map_err(|e| e.to_string())?;
        let w = dir
            .dev
            .create(&dir, &leaf, perm)
            .map_err(|e| e.to_string())?;
        let mut c = Chan::new(dir.dev.clone(), dir.devno, w.qid, String::new(), w.aux);
        c.name = absolute(&path, "/");
        let opened = open(c, mode).map_err(|e| e.to_string())?;
        let qid = opened.qid;
        self.fids.insert(
            fid,
            Fid {
                path: opened.name.clone(),
                chan: opened,
            },
        );
        Ok((qid, self.iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let mut buf = alloc::vec![0u8; count as usize];
        let n = if f.chan.is_dir() {
            sysfile::dir_read(&f.chan, &mut buf, offset)
        } else {
            f.chan.dev.read(&f.chan, &mut buf, offset)
        }
        .map_err(|e| e.to_string())?;
        buf.truncate(n);
        Ok(buf)
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let n = f
            .chan
            .dev
            .write(&f.chan, data, offset)
            .map_err(|e| e.to_string())?;
        u32::try_from(n).map_err(|_| String::from(error::EIO))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        f.chan.dev.remove(&f.chan).map_err(|e| e.to_string())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let d = f.chan.dev.stat(&f.chan).map_err(|e| e.to_string())?;
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        let d = Dir::unpack(stat)
            .map(|(d, _)| d)
            .map_err(|e| e.to_string())?;
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        f.chan.dev.wstat(&f.chan, &d).map_err(|e| e.to_string())
    }
}
