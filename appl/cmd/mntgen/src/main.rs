//! mntgen: serve, on fd 0, a directory in which every name exists. Walking
//! to a name makes an empty directory of that name, which lasts while some
//! fid refers to it, so `mount {mntgen} /n` gives a mount point for any
//! `/n/name`. Reading the directory lists the names some fid refers to; a
//! name that is only mounted on is not listed, since the kernel's mount
//! table holds no fid on it.

use std::collections::BTreeMap;
use std::os::fd::FromRawFd;

use purgatorio::abi::{error, mode};
use purgatorio::styx::{self, IOHDRSZ, StyxFs};
use purgatorio::{Context, Error};
use purgatorio::{DMDIR, Dir, Qid};

const QTDIR: u8 = 0x80;

/// The root's qid path; names get paths from 1.
const ROOT: u64 = 0;

struct Name {
    path: u64,
    refs: usize,
}

#[derive(Clone)]
struct Fid {
    /// `None` is the root.
    name: Option<String>,
    /// The packed listing a read of the root returns, made at offset 0.
    snap: Vec<u8>,
}

struct Mntgen {
    names: BTreeMap<String, Name>,
    fids: BTreeMap<u32, Fid>,
    next: u64,
    iounit: u32,
}

impl Mntgen {
    fn new() -> Mntgen {
        Mntgen {
            names: BTreeMap::new(),
            fids: BTreeMap::new(),
            next: 1,
            iounit: 8192,
        }
    }

    fn qid(path: u64) -> Qid {
        Qid {
            path,
            vers: 0,
            qtype: QTDIR,
        }
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn path_of(&self, name: &Option<String>) -> u64 {
        name.as_ref()
            .and_then(|n| self.names.get(n))
            .map_or(ROOT, |n| n.path)
    }

    /// The entry for `name`, made on first use. Its qid path never changes,
    /// because the kernel's mount table knows a mount point by qid and
    /// keeps no fid on it.
    fn name(&mut self, name: &str) -> &mut Name {
        let next = &mut self.next;
        self.names.entry(String::from(name)).or_insert_with(|| {
            let path = *next;
            *next += 1;
            Name { path, refs: 0 }
        })
    }

    fn release(&mut self, name: &Option<String>) {
        let Some(name) = name else { return };
        if let Some(n) = self.names.get_mut(name) {
            n.refs -= 1;
        }
    }

    fn set(&mut self, fid: u32, f: Fid) {
        if let Some(old) = self.fids.insert(fid, f) {
            self.release(&old.name);
        }
    }

    fn dir(name: &str, path: u64) -> Dir {
        Dir {
            qid: Mntgen::qid(path),
            mode: DMDIR | 0o555,
            name: String::from(name),
            uid: String::from("inferno"),
            gid: String::from("inferno"),
            muid: String::from("inferno"),
            ..Dir::default()
        }
    }

    fn listing(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, n) in self.names.iter().filter(|(_, n)| n.refs > 0) {
            let _ = Mntgen::dir(name, n.path).pack_into(&mut out);
        }
        out
    }
}

impl StyxFs for Mntgen {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.set(
            fid,
            Fid {
                name: None,
                snap: Vec::new(),
            },
        );
        Ok(Mntgen::qid(ROOT))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let mut at = self.fid(fid)?.name.clone();
        let mut qids = Vec::new();
        for name in names {
            at = match (&at, *name) {
                (None, "..") => None,
                (None, n) => Some(String::from(n)),
                (Some(_), "..") => None,
                // A made directory is empty.
                (Some(_), _) => {
                    if qids.is_empty() {
                        return Err(String::from(error::ENONEXIST));
                    }
                    return Ok(qids);
                }
            };
            let path = match &at {
                None => ROOT,
                Some(n) => self.name(n).path,
            };
            qids.push(Mntgen::qid(path));
        }
        if let Some(n) = &at {
            self.name(n).refs += 1;
        }
        self.set(
            newfid,
            Fid {
                name: at,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let f = self.fid(fid)?;
        if omode & 3 != mode::OREAD || omode & mode::OTRUNC != 0 {
            return Err(String::from(error::EPERM));
        }
        Ok((Mntgen::qid(self.path_of(&f.name)), self.iounit))
    }

    fn create(
        &mut self,
        _fid: u32,
        _name: &str,
        _perm: u32,
        _mode: u8,
    ) -> Result<(Qid, u32), String> {
        Err(String::from(error::EPERM))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        if f.name.is_some() {
            return Ok(Vec::new());
        }
        if offset == 0 {
            let snap = self.listing();
            if let Some(f) = self.fids.get_mut(&fid) {
                f.snap = snap;
            }
        }
        let snap = &self.fid(fid)?.snap;
        let start = (offset as usize).min(snap.len());
        let end = (start + count as usize).min(snap.len());
        // Whole entries only.
        let mut n = start;
        while n + 2 <= end {
            let len = u16::from_le_bytes([snap[n], snap[n + 1]]) as usize + 2;
            if n + len > end {
                break;
            }
            n += len;
        }
        Ok(snap[start..n].to_vec())
    }

    fn write(&mut self, _fid: u32, _offset: u64, _data: &[u8]) -> Result<u32, String> {
        Err(String::from(error::EPERM))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        self.release(&f.name);
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(String::from(error::EPERM))
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        let d = match &f.name {
            None => Mntgen::dir("/", ROOT),
            Some(n) => Mntgen::dir(n, self.path_of(&f.name)),
        };
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() > 1 {
        return Err(Error::from("usage: mntgen"));
    }
    let io = unsafe { std::fs::File::from_raw_fd(0) };
    styx::serve(io, &mut Mntgen::new())
}
