//! A Styx server for a guest: [`serve`] turns an fd into 9P, and [`Tree`] is
//! an in-memory filesystem a command such as `memfs` can mount.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use purg_abi::mode;
use purg_styx::server::{MAX_MSIZE, dispatch};
use purg_styx::{DMDIR, Dir, Msg, QTDIR, QTFILE, Qid};

use crate::Error;

/// The operations a server implements; [`serve`] drives one.
pub use purg_styx::server::{IOHDRSZ, StyxFs};

struct Node {
    dir: bool,
    mode: u32,
    data: Vec<u8>,
    parent: u64,
    name: String,
    children: BTreeMap<String, u64>,
}

struct Fid {
    qid: u64,
    /// Packed directory entries, rebuilt when a read starts at offset 0.
    snap: Vec<u8>,
}

/// An empty in-memory tree. Its root qid path is 0.
pub struct Tree {
    nodes: BTreeMap<u64, Node>,
    fids: BTreeMap<u32, Fid>,
    next: u64,
    iounit: u32,
}

impl Tree {
    pub fn new() -> Tree {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            0,
            Node {
                dir: true,
                mode: DMDIR | 0o775,
                data: Vec::new(),
                parent: 0,
                name: String::from("/"),
                children: BTreeMap::new(),
            },
        );
        Tree {
            nodes,
            fids: BTreeMap::new(),
            next: 1,
            iounit: 8192,
        }
    }

    fn dir_of(&self, qid: u64) -> Vec<u8> {
        let Some(node) = self.nodes.get(&qid) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, child) in &node.children {
            if let Some(n) = self.nodes.get(child) {
                let _ = self.stat_of(*child, name, n).pack_into(&mut out);
            }
        }
        out
    }

    fn stat_of(&self, qid: u64, name: &str, n: &Node) -> Dir {
        Dir {
            qid: Qid {
                path: qid,
                vers: 0,
                qtype: if n.dir { QTDIR } else { QTFILE },
            },
            mode: n.mode,
            length: if n.dir { 0 } else { n.data.len() as u64 },
            name: String::from(name),
            uid: String::from("inferno"),
            gid: String::from("inferno"),
            muid: String::from("inferno"),
            ..Dir::default()
        }
    }
}

impl Default for Tree {
    fn default() -> Tree {
        Tree::new()
    }
}

impl StyxFs for Tree {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(purg_styx::IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                qid: 0,
                snap: Vec::new(),
            },
        );
        Ok(Qid {
            path: 0,
            vers: 0,
            qtype: QTDIR,
        })
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let mut qid = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        if names.is_empty() {
            self.fids.insert(
                newfid,
                Fid {
                    qid,
                    snap: Vec::new(),
                },
            );
            return Ok(Vec::new());
        }
        let mut walked = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let next = if *name == ".." {
                self.nodes.get(&qid).map(|n| n.parent)
            } else {
                self.nodes
                    .get(&qid)
                    .and_then(|n| n.children.get(*name).copied())
            };
            match next {
                Some(q) => {
                    let qt = self.nodes.get(&q).map(|n| n.dir).unwrap_or(false);
                    walked.push(Qid {
                        path: q,
                        vers: 0,
                        qtype: if qt { QTDIR } else { QTFILE },
                    });
                    qid = q;
                }
                None => {
                    if i == 0 {
                        return Err(String::from(purg_abi::error::ENONEXIST));
                    }
                    return Ok(walked);
                }
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                qid,
                snap: Vec::new(),
            },
        );
        Ok(walked)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let qid = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        let node = self
            .nodes
            .get_mut(&qid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if !node.dir && omode & mode::OTRUNC != 0 {
            node.data.clear();
        }
        let dir = node.dir;
        Ok((
            Qid {
                path: qid,
                vers: 0,
                qtype: if dir { QTDIR } else { QTFILE },
            },
            self.iounit,
        ))
    }

    fn create(
        &mut self,
        fid: u32,
        name: &str,
        perm: u32,
        mode_bits: u8,
    ) -> Result<(Qid, u32), String> {
        if name.is_empty() || name.contains('/') {
            return Err(String::from(purg_abi::error::EBADARG));
        }
        let parent = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        if self
            .nodes
            .get(&parent)
            .is_some_and(|n| n.children.contains_key(name))
        {
            return Err(String::from(purg_abi::error::EEXIST));
        }
        let qid = self.next;
        self.next += 1;
        let dir = perm & DMDIR != 0;
        self.nodes.insert(
            qid,
            Node {
                dir,
                mode: perm,
                data: Vec::new(),
                parent,
                name: String::from(name),
                children: BTreeMap::new(),
            },
        );
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.insert(String::from(name), qid);
        }
        self.fids.insert(
            fid,
            Fid {
                qid,
                snap: Vec::new(),
            },
        );
        let _ = mode_bits;
        Ok((
            Qid {
                path: qid,
                vers: 0,
                qtype: if dir { QTDIR } else { QTFILE },
            },
            self.iounit,
        ))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let qid = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        let is_dir = self
            .nodes
            .get(&qid)
            .ok_or_else(|| String::from("unknown fid"))?
            .dir;
        if is_dir {
            if offset == 0 {
                let snap = self.dir_of(qid);
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.snap = snap;
                }
            }
            let snap = self
                .fids
                .get(&fid)
                .map(|f| f.snap.clone())
                .unwrap_or_default();
            let start = offset as usize;
            if start >= snap.len() {
                return Ok(Vec::new());
            }
            let end = (start + count as usize).min(snap.len());
            // Return only whole stat entries.
            let mut n = start;
            while n < end {
                if n + 2 > snap.len() {
                    break;
                }
                let len = u16::from_le_bytes([snap[n], snap[n + 1]]) as usize + 2;
                if n + len > end {
                    break;
                }
                n += len;
            }
            return Ok(snap[start..n].to_vec());
        }
        let data = &self
            .nodes
            .get(&qid)
            .ok_or_else(|| String::from("unknown fid"))?
            .data;
        let start = offset as usize;
        if start >= data.len() {
            return Ok(Vec::new());
        }
        let end = (start + count as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let qid = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        let node = self
            .nodes
            .get_mut(&qid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if node.dir {
            return Err(String::from(purg_abi::error::EISDIR));
        }
        let off = offset as usize;
        if off > node.data.len() {
            node.data.resize(off, 0);
        }
        let end = off + data.len();
        if end > node.data.len() {
            node.data.resize(end, 0);
        }
        node.data[off..end].copy_from_slice(data);
        u32::try_from(data.len()).map_err(|_| String::from(purg_abi::error::EIO))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let qid = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        let Some(node) = self.nodes.remove(&qid) else {
            return Ok(());
        };
        if node.dir && !node.children.is_empty() {
            self.nodes.insert(qid, node);
            return Err(String::from(purg_abi::error::ENOTEMPTY));
        }
        if let Some(parent) = self.nodes.get_mut(&node.parent) {
            parent.children.remove(&node.name);
        }
        Ok(())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let qid = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        let node = self
            .nodes
            .get(&qid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let name = node.name.clone();
        let dir = self.stat_of(qid, &name, node);
        dir.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        let d = Dir::unpack(stat)
            .map(|(d, _)| d)
            .map_err(|e| e.to_string())?;
        let qid = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?
            .qid;
        let parent = self.nodes.get(&qid).map(|n| n.parent).unwrap_or(0);
        let old_name = self
            .nodes
            .get(&qid)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        if !d.name.is_empty() && d.name != old_name {
            if self
                .nodes
                .get(&parent)
                .is_some_and(|p| p.children.contains_key(&d.name))
            {
                return Err(String::from(purg_abi::error::EEXIST));
            }
            if let Some(p) = self.nodes.get_mut(&parent) {
                p.children.remove(&old_name);
                p.children.insert(d.name.clone(), qid);
            }
            if let Some(n) = self.nodes.get_mut(&qid) {
                n.name = d.name;
            }
        }
        if let Some(n) = self.nodes.get_mut(&qid) {
            if d.length != u64::MAX && !n.dir {
                n.data.resize(d.length as usize, 0);
            }
            if d.mode != u32::MAX {
                n.mode = d.mode;
            }
        }
        Ok(())
    }
}

/// Speaks 9P2000 on `io` until the client closes it.
pub fn serve(mut io: impl Read + Write, fs: &mut dyn StyxFs) -> Result<(), Error> {
    let mut msize = MAX_MSIZE;
    loop {
        let buf = match read_msg(&mut io, msize) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(Error(e.to_string())),
        };
        let msg = Msg::decode(&buf, msize).map_err(|e| Error(e.to_string()))?;
        let reply = dispatch(fs, &msg, &mut msize);
        io.write_all(&reply).map_err(|e| Error(e.to_string()))?;
    }
}

fn read_msg(io: &mut impl Read, msize: u32) -> io::Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    io.read_exact(&mut hdr)?;
    let n = u32::from_le_bytes(hdr) as usize;
    if n < 7 || n > msize as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad 9P size"));
    }
    let mut buf = vec![0u8; n];
    buf[..4].copy_from_slice(&hdr);
    io.read_exact(&mut buf[4..])?;
    Ok(buf)
}
