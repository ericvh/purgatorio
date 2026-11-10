//! A read-only tree of files held in memory, for servers of archives
//! (`tarfs`, `archfs`). Each file keeps the `Dir` the archive gave it.

use std::collections::BTreeMap;

use purg_styx::{DMDIR, Dir, IOHDRSZ, QTDIR, QTFILE, Qid, StyxFs};
use purgatorio::abi::{error, mode};

struct Node {
    dir: Dir,
    data: Vec<u8>,
    parent: usize,
    children: BTreeMap<String, usize>,
}

#[derive(Clone)]
struct Fid {
    node: usize,
    open: bool,
    /// The packed listing a directory read returns, made at offset 0.
    snap: Vec<u8>,
}

/// A read-only tree. Node 0 is the root.
pub struct RoTree {
    nodes: Vec<Node>,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

fn qid(index: usize, d: &Dir) -> Qid {
    Qid {
        path: index as u64,
        vers: 0,
        qtype: if d.mode & DMDIR != 0 { QTDIR } else { QTFILE },
    }
}

impl RoTree {
    /// A tree holding only a root directory described by `root` (its name
    /// is replaced by `/`).
    pub fn new(mut root: Dir) -> RoTree {
        root.name = String::from("/");
        root.mode |= DMDIR;
        root.length = 0;
        root.qid = qid(0, &root);
        RoTree {
            nodes: vec![Node {
                dir: root,
                data: Vec::new(),
                parent: 0,
                children: BTreeMap::new(),
            }],
            fids: BTreeMap::new(),
            iounit: 8192,
        }
    }

    /// The node for `path`, relative to the root; empty and `.` elements
    /// are skipped and `..` goes up.
    pub fn lookup(&self, path: &str) -> Option<usize> {
        let mut at = 0;
        for e in path.split('/') {
            match e {
                "" | "." => {}
                ".." => at = self.nodes[at].parent,
                _ => at = *self.nodes[at].children.get(e)?,
            }
        }
        Some(at)
    }

    /// Adds the file `path` with metadata `d` and contents `data`. Missing
    /// directories on the way are made with the root's owner and `d`'s
    /// times. Adding a directory that exists replaces its metadata; adding
    /// a file that exists replaces it, as a later archive member does.
    pub fn add(&mut self, path: &str, mut d: Dir, data: Vec<u8>) {
        let elems: Vec<&str> = path
            .split('/')
            .filter(|e| !e.is_empty() && *e != "." && *e != "..")
            .collect();
        let Some((last, dirs)) = elems.split_last() else {
            return;
        };
        let mut at = 0;
        for e in dirs {
            at = match self.nodes[at].children.get(*e) {
                Some(&n) if self.nodes[n].dir.mode & DMDIR != 0 => n,
                _ => {
                    let root = &self.nodes[0].dir;
                    let made = Dir {
                        mode: DMDIR | 0o555,
                        mtime: d.mtime,
                        atime: d.mtime,
                        name: String::from(*e),
                        uid: root.uid.clone(),
                        gid: root.gid.clone(),
                        muid: root.muid.clone(),
                        ..Dir::default()
                    };
                    self.insert(at, made, Vec::new())
                }
            };
        }
        d.name = String::from(*last);
        if d.mode & DMDIR != 0 {
            d.length = 0;
            if let Some(&n) = self.nodes[at].children.get(*last)
                && self.nodes[n].dir.mode & DMDIR != 0
            {
                d.qid = qid(n, &d);
                self.nodes[n].dir = d;
                return;
            }
        } else {
            d.length = data.len() as u64;
        }
        self.insert(at, d, data);
    }

    fn insert(&mut self, parent: usize, mut d: Dir, data: Vec<u8>) -> usize {
        let n = self.nodes.len();
        d.qid = qid(n, &d);
        let name = d.name.clone();
        self.nodes.push(Node {
            dir: d,
            data,
            parent,
            children: BTreeMap::new(),
        });
        self.nodes[parent].children.insert(name, n);
        n
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn listing(&self, node: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for &c in self.nodes[node].children.values() {
            let _ = self.nodes[c].dir.pack_into(&mut out);
        }
        out
    }
}

/// The whole stat entries of `snap` that fit in `count` bytes from
/// `offset`.
pub fn dir_slice(snap: &[u8], offset: u64, count: u32) -> Vec<u8> {
    let start = (offset as usize).min(snap.len());
    let end = start.saturating_add(count as usize).min(snap.len());
    let mut n = start;
    while n + 2 <= end {
        let len = u16::from_le_bytes([snap[n], snap[n + 1]]) as usize + 2;
        if n + len > end {
            break;
        }
        n += len;
    }
    snap[start..n].to_vec()
}

impl StyxFs for RoTree {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                node: 0,
                open: false,
                snap: Vec::new(),
            },
        );
        Ok(self.nodes[0].dir.qid)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let f = self.fid(fid)?;
        let mut at = f.node;
        let mut qids = Vec::new();
        for name in names {
            let node = &self.nodes[at];
            if node.dir.mode & DMDIR == 0 {
                if qids.is_empty() {
                    return Err(String::from(error::ENOTDIR));
                }
                return Ok(qids);
            }
            let next = if *name == ".." {
                Some(node.parent)
            } else {
                node.children.get(*name).copied()
            };
            match next {
                Some(n) => {
                    at = n;
                    qids.push(self.nodes[n].dir.qid);
                }
                None if qids.is_empty() => return Err(String::from(error::ENONEXIST)),
                None => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                node: at,
                open: false,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let node = self.fid(fid)?.node;
        let access = omode & mode::ACCESS;
        if (access != mode::OREAD && access != mode::OEXEC)
            || omode & (mode::OTRUNC | mode::ORCLOSE) != 0
        {
            return Err(String::from(error::EPERM));
        }
        if let Some(f) = self.fids.get_mut(&fid) {
            f.open = true;
        }
        Ok((self.nodes[node].dir.qid, self.iounit))
    }

    fn create(&mut self, _fid: u32, _name: &str, _perm: u32, _m: u8) -> Result<(Qid, u32), String> {
        Err(String::from(error::EPERM))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        let node = f.node;
        if self.nodes[node].dir.mode & DMDIR != 0 {
            if offset == 0 {
                let snap = self.listing(node);
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.snap = snap;
                }
            }
            return Ok(dir_slice(&self.fid(fid)?.snap, offset, count));
        }
        let data = &self.nodes[node].data;
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(count as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }

    fn write(&mut self, _fid: u32, _offset: u64, _data: &[u8]) -> Result<u32, String> {
        Err(String::from(error::EPERM))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .map(drop)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(String::from(error::EPERM))
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let node = self.fid(fid)?.node;
        self.nodes[node].dir.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(mode: u32) -> Dir {
        Dir {
            mode,
            mtime: 7,
            uid: String::from("u"),
            ..Dir::default()
        }
    }

    #[test]
    fn build_and_walk() {
        let mut t = RoTree::new(file(0o555));
        t.add("a/b/c", file(0o644), b"hello".to_vec());
        t.add("./a/d", file(DMDIR | 0o700), Vec::new());
        t.add("a", file(DMDIR | 0o750), Vec::new());
        let c = t.lookup("a/b/c").unwrap();
        assert_eq!(t.nodes[c].dir.length, 5);
        assert_eq!(t.nodes[t.lookup("a").unwrap()].dir.mode, DMDIR | 0o750);
        assert_eq!(
            t.lookup("a/b/../d").map(|n| t.nodes[n].dir.mode),
            Some(DMDIR | 0o700)
        );
        t.attach(1, !0, "", "").unwrap();
        assert_eq!(t.walk(1, 2, &["a", "x"]).unwrap().len(), 1);
        assert!(t.fids.get(&2).is_none());
        assert_eq!(t.walk(1, 2, &["a", "b", "c"]).unwrap().len(), 3);
        assert_eq!(t.open(2, mode::OWRITE), Err(String::from(error::EPERM)));
        t.open(2, mode::OREAD).unwrap();
        assert_eq!(t.read(2, 1, 3).unwrap(), b"ell");
        t.walk(1, 3, &["a"]).unwrap();
        t.open(3, mode::OREAD).unwrap();
        let l = t.read(3, 0, 8192).unwrap();
        let names: Vec<String> = purg_styx::dirs(&l).map(|d| d.unwrap().name).collect();
        assert_eq!(names, ["b", "d"]);
    }
}
