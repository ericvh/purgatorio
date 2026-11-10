//! A server that passes a directory of the namespace through as 9P, doing
//! each request with ordinary file calls. A [`Policy`] sits in between:
//! `trfs` renames, `lockfs` refuses some opens.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

use purg_styx::{Dir, IOHDRSZ, Qid, StyxFs};
use purgatorio::abi::error;

use crate::ioerr;
use crate::rotree::dir_slice;

/// What a pass-through server does besides passing requests on.
pub trait Policy {
    /// The inner name for a name a client used.
    fn to_inner(&self, name: &str) -> String {
        String::from(name)
    }
    /// The name a client sees for an inner name.
    fn to_outer(&self, name: &str) -> String {
        String::from(name)
    }
    /// Called before a file is opened or after it is created: an error
    /// refuses the open. `d` is the file's inner `Dir`.
    fn open(&mut self, _fid: u32, _d: &Dir, _mode: u8) -> Result<(), String> {
        Ok(())
    }
    /// Called when an opened fid is clunked or removed.
    fn close(&mut self, _fid: u32) {}
}

/// A [`Policy`] that changes nothing.
pub struct Plain;

impl Policy for Plain {}

struct Fid {
    /// Inner names from the root.
    elems: Vec<String>,
    qid: Qid,
    file: Option<File>,
    /// The packed listing a directory read returns, made at offset 0.
    snap: Vec<u8>,
}

/// Serves the tree at `root` through `policy`.
pub struct Pass<P: Policy> {
    root: String,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
    pub policy: P,
}

impl<P: Policy> Pass<P> {
    pub fn new(root: &str, policy: P) -> Pass<P> {
        Pass {
            root: String::from(root.trim_end_matches('/')),
            fids: BTreeMap::new(),
            iounit: 8192,
            policy,
        }
    }

    fn path(&self, elems: &[String]) -> String {
        let mut p = self.root.clone();
        for e in elems {
            p.push('/');
            p.push_str(e);
        }
        if p.is_empty() {
            p.push('/');
        }
        p
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn fid_mut(&mut self, fid: u32) -> Result<&mut Fid, String> {
        self.fids
            .get_mut(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn outer(&self, mut d: Dir) -> Dir {
        d.name = self.policy.to_outer(&d.name);
        d
    }

    /// The directory's entries, renamed for the client.
    fn listing(&self, path: &str) -> Result<Vec<u8>, String> {
        let fd = purgatorio::open(path, purgatorio::abi::mode::OREAD).map_err(|e| e.0)?;
        let mut out = Vec::new();
        loop {
            let ds = purgatorio::dirread(fd.as_raw_fd()).map_err(|e| e.0)?;
            if ds.is_empty() {
                return Ok(out);
            }
            for d in ds {
                let _ = self.outer(d).pack_into(&mut out);
            }
        }
    }
}

impl<P: Policy> StyxFs for Pass<P> {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        let d = purgatorio::stat(&self.path(&[])).map_err(|e| e.0)?;
        self.fids.insert(
            fid,
            Fid {
                elems: Vec::new(),
                qid: d.qid,
                file: None,
                snap: Vec::new(),
            },
        );
        Ok(d.qid)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let f = self.fid(fid)?;
        let mut elems = f.elems.clone();
        let mut qid = f.qid;
        let mut qids = Vec::new();
        for name in names {
            if !qid.is_dir() {
                if qids.is_empty() {
                    return Err(String::from(error::ENOTDIR));
                }
                return Ok(qids);
            }
            let mut next = elems.clone();
            if *name == ".." {
                next.pop();
            } else {
                next.push(self.policy.to_inner(name));
            }
            match purgatorio::stat(&self.path(&next)) {
                Ok(d) => {
                    elems = next;
                    qid = d.qid;
                    qids.push(qid);
                }
                Err(e) if qids.is_empty() => return Err(e.0),
                Err(_) => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                elems,
                qid,
                file: None,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, mode: u8) -> Result<(Qid, u32), String> {
        let path = self.path(&self.fid(fid)?.elems);
        let d = purgatorio::stat(&path).map_err(|e| e.0)?;
        self.policy.open(fid, &d, mode)?;
        let file = match purgatorio::open(&path, mode) {
            Ok(fd) => File::from(fd),
            Err(e) => {
                self.policy.close(fid);
                return Err(e.0);
            }
        };
        let iounit = self.iounit;
        let f = self.fid_mut(fid)?;
        f.file = Some(file);
        f.qid = d.qid;
        Ok((d.qid, iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, mode: u8) -> Result<(Qid, u32), String> {
        if name == "." || name == ".." || name.contains('/') {
            return Err(String::from(error::EFILENAME));
        }
        let mut elems = self.fid(fid)?.elems.clone();
        elems.push(self.policy.to_inner(name));
        let path = self.path(&elems);
        let file = File::from(purgatorio::create(&path, mode, perm).map_err(|e| e.0)?);
        let d = purgatorio::fstat(file.as_raw_fd()).map_err(|e| e.0)?;
        self.policy.open(fid, &d, mode)?;
        let iounit = self.iounit;
        let f = self.fid_mut(fid)?;
        f.elems = elems;
        f.qid = d.qid;
        f.file = Some(file);
        Ok((d.qid, iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        if f.file.is_none() {
            return Err(String::from("fid not open"));
        }
        if f.qid.is_dir() {
            if offset == 0 {
                let snap = self.listing(&self.path(&f.elems))?;
                self.fid_mut(fid)?.snap = snap;
            }
            return Ok(dir_slice(&self.fid(fid)?.snap, offset, count));
        }
        let file = self.fid_mut(fid)?.file.as_mut().ok_or("fid not open")?;
        // A stream cannot seek; its reads come in order anyway.
        let _ = file.seek(SeekFrom::Start(offset));
        let mut buf = vec![0u8; count as usize];
        let n = file.read(&mut buf).map_err(ioerr)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let file = self.fid_mut(fid)?.file.as_mut().ok_or("fid not open")?;
        let _ = file.seek(SeekFrom::Start(offset));
        let n = file.write(data).map_err(ioerr)?;
        u32::try_from(n).map_err(|_| String::from(error::EIO))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if f.file.is_some() {
            self.policy.close(fid);
        }
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let path = self.path(&self.fid(fid)?.elems);
        // Close first, so a server that refuses to remove an open file
        // does not see this fid's open.
        self.clunk(fid)?;
        purgatorio::remove(&path).map_err(|e| e.0)
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        let d = match &f.file {
            Some(file) => purgatorio::fstat(file.as_raw_fd()),
            None => purgatorio::stat(&self.path(&f.elems)),
        }
        .map_err(|e| e.0)?;
        self.outer(d).pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        let (mut d, _) = Dir::unpack(stat).map_err(|e| e.to_string())?;
        let f = self.fid(fid)?;
        let mut elems = f.elems.clone();
        if !d.name.is_empty() {
            d.name = self.policy.to_inner(&d.name);
            if elems.last() == Some(&d.name) {
                d.name.clear();
            }
        }
        match &f.file {
            Some(file) => purgatorio::fwstat(file.as_raw_fd(), &d),
            None => purgatorio::wstat(&self.path(&elems), &d),
        }
        .map_err(|e| e.0)?;
        if !d.name.is_empty() && !elems.is_empty() {
            elems.pop();
            elems.push(d.name);
            self.fid_mut(fid)?.elems = elems;
        }
        Ok(())
    }
}
