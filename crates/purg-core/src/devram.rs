//! A writable tree held in memory, for systems without a host file system.
//! The bare-metal probe serves its root from one, seeded with the files
//! embedded in the image; seeded contents stay borrowed until written.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::Platform;
use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};
use crate::initrd::Kind;

const ROOT: u64 = 0;

struct Node {
    name: String,
    parent: u64,
    /// Permission bits, with `DMDIR` for a directory.
    perm: u32,
    data: Cow<'static, [u8]>,
    vers: u32,
    mtime: u32,
}

impl Node {
    fn is_dir(&self) -> bool {
        self.perm & DMDIR != 0
    }
}

struct Tree {
    nodes: BTreeMap<u64, Node>,
    next: u64,
}

impl Tree {
    fn get(&self, path: u64) -> Result<&Node> {
        self.nodes.get(&path).ok_or(Error::new(error::ENONEXIST))
    }

    fn get_mut(&mut self, path: u64) -> Result<&mut Node> {
        self.nodes
            .get_mut(&path)
            .ok_or(Error::new(error::ENONEXIST))
    }

    fn child(&self, parent: u64, name: &str) -> Option<u64> {
        self.nodes
            .iter()
            .find(|&(&p, n)| p != ROOT && n.parent == parent && n.name == name)
            .map(|(&p, _)| p)
    }

    fn children(&self, parent: u64) -> impl Iterator<Item = (u64, &Node)> {
        self.nodes
            .iter()
            .filter(move |&(&p, n)| p != ROOT && n.parent == parent)
            .map(|(&p, n)| (p, n))
    }

    fn add(
        &mut self,
        parent: u64,
        name: &str,
        perm: u32,
        data: Cow<'static, [u8]>,
        mtime: u32,
    ) -> u64 {
        let path = self.next;
        self.next += 1;
        self.nodes.insert(
            path,
            Node {
                name: String::from(name),
                parent,
                perm,
                data,
                vers: 0,
                mtime,
            },
        );
        path
    }
}

/// The RAM tree device.
pub struct RamTree {
    dc: char,
    name: &'static str,
    owner: String,
    platform: Arc<dyn Platform>,
    tree: Mutex<Tree>,
}

fn qid(path: u64, n: &Node) -> Qid {
    Qid {
        path,
        vers: n.vers,
        qtype: if n.is_dir() { QTDIR } else { QTFILE },
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Error::new(error::EBADARG));
    }
    Ok(())
}

impl RamTree {
    /// An empty tree served as `#<dc>`, owned by `owner`.
    pub fn new(dc: char, name: &'static str, owner: &str, platform: Arc<dyn Platform>) -> RamTree {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT,
            Node {
                name: String::from("/"),
                parent: ROOT,
                perm: DMDIR | 0o775,
                data: Cow::Borrowed(&[]),
                vers: 0,
                mtime: 0,
            },
        );
        RamTree {
            dc,
            name,
            owner: String::from(owner),
            platform,
            tree: Mutex::new(Tree { nodes, next: 1 }),
        }
    }

    fn now(&self) -> u32 {
        (self.platform.now_ns() / 1_000_000_000) as u32
    }

    /// Adds a directory, and any missing parents, at a `/`-separated path.
    pub fn add_dir(&self, path: &str) -> u64 {
        let now = self.now();
        let mut t = self.tree.lock();
        let mut cur = ROOT;
        for e in path.split('/').filter(|e| !e.is_empty()) {
            cur = match t.child(cur, e) {
                Some(p) => p,
                None => t.add(cur, e, DMDIR | 0o775, Cow::Borrowed(&[]), now),
            };
        }
        cur
    }

    /// Adds a file with initial contents, creating parent directories.
    pub fn add_file(&self, path: &str, data: impl Into<Cow<'static, [u8]>>, perm: u32) {
        let path = path.trim_start_matches('/');
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (self.add_dir(d), n),
            None => (ROOT, path),
        };
        let now = self.now();
        self.tree
            .lock()
            .add(dir, name, perm & 0o777, data.into(), now);
    }

    /// Adds every directory and regular file in a ustar archive (see
    /// [`crate::initrd`]) with its permission bits, and returns how many
    /// entries it added. File contents borrow `archive` until written, so a
    /// boot image's initrd is never copied.
    ///
    /// The archive is checked in full first: a malformed one adds nothing.
    /// An entry that replaces an existing file takes its contents and
    /// permissions; a file in the archive where the tree has a directory, or
    /// the reverse, fails with `file already exists`, possibly after earlier
    /// entries were added. Parent directories missing from the archive are
    /// created with mode 0775, like [`RamTree::add_dir`].
    pub fn add_tar(&self, archive: &'static [u8]) -> Result<usize> {
        let entries = crate::initrd::entries(archive)?;
        let now = self.now();
        let mut t = self.tree.lock();
        for e in &entries {
            let mut cur = ROOT;
            let mut parts = e.path.split('/').peekable();
            while let Some(name) = parts.next() {
                let last = parts.peek().is_none();
                let kind = if last { e.kind } else { Kind::Dir };
                let perm = match kind {
                    Kind::Dir if last => DMDIR | e.perm,
                    Kind::Dir => DMDIR | 0o775,
                    Kind::File => e.perm,
                };
                cur = match t.child(cur, name) {
                    None => t.add(cur, name, perm, Cow::Borrowed(e.data), now),
                    Some(p) => {
                        let n = t.get_mut(p)?;
                        if n.is_dir() != (kind == Kind::Dir) {
                            return Err(Error::new(error::EEXIST));
                        }
                        if last {
                            n.perm = perm;
                            if kind == Kind::File {
                                n.data = Cow::Borrowed(e.data);
                                n.vers += 1;
                                n.mtime = now;
                            }
                        }
                        p
                    }
                };
            }
        }
        Ok(entries.len())
    }

    fn dir(&self, path: u64, n: &Node) -> Dir {
        Dir {
            dtype: self.dc as u16,
            dev: 0,
            qid: qid(path, n),
            mode: n.perm,
            atime: n.mtime,
            mtime: n.mtime,
            length: if n.is_dir() { 0 } else { n.data.len() as u64 },
            name: n.name.clone(),
            uid: self.owner.clone(),
            gid: self.owner.clone(),
            muid: self.owner.clone(),
        }
    }
}

impl Dev for RamTree {
    fn dc(&self) -> char {
        self.dc
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        let t = self.tree.lock();
        Ok(Walked {
            qid: qid(ROOT, t.get(ROOT)?),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let t = self.tree.lock();
        if !t.get(c.qid.path)?.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        let p = t
            .child(c.qid.path, name)
            .ok_or(Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: qid(p, t.get(p)?),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let t = self.tree.lock();
        Ok(self.dir(c.qid.path, t.get(c.qid.path)?))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let now = self.now();
        let mut t = self.tree.lock();
        let n = t.get_mut(c.qid.path)?;
        let write = matches!(omode & mode::ACCESS, mode::OWRITE | mode::ORDWR);
        if !n.is_dir() && write && omode & mode::OTRUNC != 0 {
            n.data = Cow::Owned(Vec::new());
            n.vers = n.vers.wrapping_add(1);
            n.mtime = now;
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let t = self.tree.lock();
        let n = t.get(c.qid.path)?;
        if n.is_dir() {
            return Err(Error::new(error::EISDIR));
        }
        Ok(read_str(&n.data, buf, offset))
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let t = self.tree.lock();
        if !t.get(c.qid.path)?.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        let mut out: Vec<Dir> = t
            .children(c.qid.path)
            .map(|(p, n)| self.dir(p, n))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let now = self.now();
        let mut t = self.tree.lock();
        let n = t.get_mut(c.qid.path)?;
        if n.is_dir() {
            return Err(Error::new(error::EISDIR));
        }
        let off = usize::try_from(offset).map_err(|_| Error::new(error::EBADARG))?;
        let end = off
            .checked_add(buf.len())
            .ok_or(Error::new(error::EBADARG))?;
        let data = n.data.to_mut();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[off..end].copy_from_slice(buf);
        n.vers = n.vers.wrapping_add(1);
        n.mtime = now;
        Ok(buf.len())
    }

    fn create(&self, dir: &Chan, name: &str, perm: u32) -> Result<Walked> {
        check_name(name)?;
        let now = self.now();
        let mut t = self.tree.lock();
        if !t.get(dir.qid.path)?.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        let p = match t.child(dir.qid.path, name) {
            Some(p) => {
                let n = t.get_mut(p)?;
                if n.is_dir() || perm & DMDIR != 0 {
                    return Err(Error::new(error::EEXIST));
                }
                n.data = Cow::Owned(Vec::new());
                n.vers = n.vers.wrapping_add(1);
                n.mtime = now;
                p
            }
            None => t.add(
                dir.qid.path,
                name,
                perm & (DMDIR | 0o777),
                Cow::Owned(Vec::new()),
                now,
            ),
        };
        Ok(Walked {
            qid: qid(p, t.get(p)?),
            aux: None,
        })
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        let path = c.qid.path;
        if path == ROOT {
            return Err(Error::new(error::EPERM));
        }
        let mut t = self.tree.lock();
        t.get(path)?;
        if t.children(path).next().is_some() {
            return Err(Error::new(error::ENOTEMPTY));
        }
        t.nodes.remove(&path);
        Ok(())
    }

    fn wstat(&self, c: &Chan, d: &Dir) -> Result<()> {
        let path = c.qid.path;
        let mut t = self.tree.lock();
        let n = t.get(path)?;
        let (parent, is_dir) = (n.parent, n.is_dir());
        if d.length != !0 && is_dir {
            return Err(Error::new(error::EISDIR));
        }
        if d.mode != !0 && (d.mode & DMDIR != 0) != is_dir {
            return Err(Error::new(error::EPERM));
        }
        let rename = !d.name.is_empty() && d.name != n.name;
        if rename {
            check_name(&d.name)?;
            if path == ROOT {
                return Err(Error::new(error::EPERM));
            }
            if t.child(parent, &d.name).is_some() {
                return Err(Error::new(error::EEXIST));
            }
        }
        let n = t.get_mut(path)?;
        if d.length != !0 {
            let len = usize::try_from(d.length).map_err(|_| Error::new(error::EBADARG))?;
            n.data.to_mut().resize(len, 0);
            n.vers = n.vers.wrapping_add(1);
        }
        if d.mode != !0 {
            n.perm = (n.perm & DMDIR) | (d.mode & 0o777);
        }
        if d.mtime != !0 {
            n.mtime = d.mtime;
        }
        if rename {
            n.name = d.name.clone();
        }
        Ok(())
    }
}
