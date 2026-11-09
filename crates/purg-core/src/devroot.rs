//! `#/`: a static, read-only tree built at boot.
//!
//! The emu uses it as the namespace root skeleton (mount points for `#c`,
//! `#U` and later devices). The bare-metal probe also uses it to serve an
//! embedded read-only root with file contents.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

struct Node {
    name: String,
    parent: usize,
    /// `None` for directories.
    data: Option<Cow<'static, [u8]>>,
}

/// A static tree device.
pub struct StaticTree {
    dc: char,
    name: &'static str,
    owner: String,
    nodes: Vec<Node>,
}

impl StaticTree {
    /// An empty tree served as `#<dc>`.
    pub fn new(dc: char, name: &'static str, owner: &str) -> StaticTree {
        StaticTree {
            dc,
            name,
            owner: String::from(owner),
            nodes: alloc::vec![Node {
                name: String::from("/"),
                parent: 0,
                data: None,
            }],
        }
    }

    /// The emu's `#/`: empty mount points for devices bound at boot.
    pub fn root(owner: &str) -> StaticTree {
        let mut t = StaticTree::new('/', "root", owner);
        for d in ["chan", "dev", "env", "fd", "net", "prog", "cmd"] {
            t.add_dir(d);
        }
        t
    }

    /// Adds a directory (and any missing parents) at a `/`-separated path.
    pub fn add_dir(&mut self, path: &str) -> usize {
        let mut cur = 0;
        for e in path.split('/').filter(|e| !e.is_empty()) {
            cur = match self.child(cur, e) {
                Some(i) => i,
                None => self.push(cur, e, None),
            };
        }
        cur
    }

    /// Adds a file with fixed contents, creating parent directories.
    pub fn add_file(&mut self, path: &str, data: impl Into<Cow<'static, [u8]>>) {
        let (dir, name) = match path.trim_start_matches('/').rsplit_once('/') {
            Some((d, n)) => (self.add_dir(d), n),
            None => (0, path.trim_start_matches('/')),
        };
        self.push(dir, name, Some(data.into()));
    }

    fn push(&mut self, parent: usize, name: &str, data: Option<Cow<'static, [u8]>>) -> usize {
        self.nodes.push(Node {
            name: String::from(name),
            parent,
            data,
        });
        self.nodes.len() - 1
    }

    fn child(&self, parent: usize, name: &str) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, n)| n.parent == parent && n.name == name)
            .map(|(i, _)| i)
    }

    fn qid(&self, i: usize) -> Qid {
        let qtype = if self.nodes[i].data.is_none() {
            QTDIR
        } else {
            QTFILE
        };
        Qid {
            path: i as u64,
            vers: 0,
            qtype,
        }
    }

    fn node(&self, c: &Chan) -> Result<usize> {
        let i = c.qid.path as usize;
        if i < self.nodes.len() {
            Ok(i)
        } else {
            Err(Error::new(error::ENONEXIST))
        }
    }

    fn dir(&self, i: usize) -> Dir {
        let n = &self.nodes[i];
        let (perm, length) = match &n.data {
            None => (DMDIR | 0o555, 0),
            Some(d) => (0o444, d.len() as u64),
        };
        Dir {
            dtype: self.dc as u16,
            dev: 0,
            qid: self.qid(i),
            mode: perm,
            name: n.name.clone(),
            uid: self.owner.clone(),
            gid: self.owner.clone(),
            length,
            ..Dir::default()
        }
    }
}

impl Dev for StaticTree {
    fn dc(&self) -> char {
        self.dc
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: self.qid(0),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let i = self.node(c)?;
        let child = self.child(i, name).ok_or(Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: self.qid(child),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(self.node(c)?))
    }

    fn open(&self, _c: &mut Chan, omode: u8) -> Result<()> {
        if omode & mode::ACCESS != mode::OREAD && omode & mode::ACCESS != mode::OEXEC {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        match &self.nodes[self.node(c)?].data {
            Some(d) => Ok(read_str(d, buf, offset)),
            None => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let i = self.node(c)?;
        Ok((1..self.nodes.len())
            .filter(|&j| self.nodes[j].parent == i)
            .map(|j| self.dir(j))
            .collect())
    }

    fn write(&self, _c: &Chan, _buf: &[u8], _offset: u64) -> Result<usize> {
        Err(Error::new(error::EPERM))
    }
}
