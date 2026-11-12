//! A read-only tree of files held in memory, for servers of archives
//! (`tarfs`, `archfs`). Each file keeps the `Dir` the archive gave it.

use std::collections::BTreeMap;

use purg_styx::{DMDIR, Dir, IOHDRSZ, QTDIR, QTFILE, Qid, StyxFs};
use purgatorio::abi::{error, mode};

struct Node {
    dir: Dir,
    data: Vec<u8>,
    /// Where the data starts in the [`Source`], for a file added with
    /// [`RoTree::add_at`].
    at: Option<u64>,
    parent: usize,
    children: BTreeMap<String, usize>,
}

#[derive(Clone)]
struct Fid {
    node: usize,
    /// The packed listing a directory read returns, made at offset 0.
    snap: Vec<u8>,
}

/// Where the data of files added with [`RoTree::add_at`] is read from.
pub trait Source {
    /// Fills `buf` from byte `off`.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String>;
}

/// A read-only tree. Node 0 is the root.
pub struct RoTree {
    nodes: Vec<Node>,
    source: Option<Box<dyn Source>>,
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
                at: None,
                parent: 0,
                children: BTreeMap::new(),
            }],
            source: None,
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
    pub fn add(&mut self, path: &str, d: Dir, data: Vec<u8>) {
        self.add_body(path, d, data, None);
    }

    /// Reads the data of files added with [`RoTree::add_at`] from `src`.
    pub fn set_source(&mut self, src: Box<dyn Source>) {
        self.source = Some(src);
    }

    /// Adds the file `path` as [`RoTree::add`] does, its `d.length` bytes
    /// of data read when wanted from byte `offset` of the source.
    pub fn add_at(&mut self, path: &str, d: Dir, offset: u64) {
        self.add_body(path, d, Vec::new(), Some(offset));
    }

    fn add_body(&mut self, path: &str, mut d: Dir, data: Vec<u8>, offset: Option<u64>) {
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
                    self.insert(at, made, Vec::new(), None)
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
        } else if offset.is_none() {
            d.length = data.len() as u64;
        }
        self.insert(at, d, data, offset);
    }

    fn insert(&mut self, parent: usize, mut d: Dir, data: Vec<u8>, at: Option<u64>) -> usize {
        let n = self.nodes.len();
        d.qid = qid(n, &d);
        let name = d.name.clone();
        self.nodes.push(Node {
            dir: d,
            data,
            at,
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
        if let Some(base) = self.nodes[node].at {
            let len = self.nodes[node].dir.length;
            let n = len.saturating_sub(offset).min(u64::from(count));
            let mut buf = vec![0u8; n as usize];
            if n > 0 {
                let src = self
                    .source
                    .as_mut()
                    .ok_or_else(|| String::from(error::EIO))?;
                src.read_at(base + offset, &mut buf)?;
            }
            return Ok(buf);
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
        assert!(!t.fids.contains_key(&2));
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
