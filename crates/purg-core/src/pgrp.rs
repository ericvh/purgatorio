//! Namespaces: Inferno's `Pgrp` and mount table, plus name resolution
//! (`namec`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use purg_abi::{error, mount};
use purg_platform::sync::{Mutex, RwLock};

use crate::chan::{Chan, Member};
use crate::dev::DevTab;
use crate::error::{Error, Result};
use crate::names::{cleanname, elements, split_device};

/// Devices a `NODEVS` namespace may still attach (Inferno's list).
const NODEVS_ALLOWED: &[char] = &['e', 's', '|', 'M'];

#[derive(Clone)]
struct Mount {
    /// Identity of the mounted-on Chan.
    key: (char, u32, u64),
    /// Union members in search order.
    to: Vec<Member>,
}

/// A namespace: a root, a mount table and the current directory.
pub struct Pgrp {
    devtab: Arc<DevTab>,
    root: Arc<Chan>,
    mounts: RwLock<Vec<Mount>>,
    dot: Mutex<String>,
    nodevs: AtomicBool,
}

impl Pgrp {
    /// A namespace rooted at `root` (usually `#/`).
    pub fn new(devtab: Arc<DevTab>, root: Chan) -> Arc<Pgrp> {
        let mut root = root;
        root.name = String::from("/");
        Arc::new(Pgrp {
            devtab,
            root: Arc::new(root),
            mounts: RwLock::new(Vec::new()),
            dot: Mutex::new(String::from("/")),
            nodevs: AtomicBool::new(false),
        })
    }

    pub fn devtab(&self) -> &Arc<DevTab> {
        &self.devtab
    }

    pub fn dot(&self) -> String {
        self.dot.lock().clone()
    }

    /// A copy of this namespace (`FORKNS`).
    pub fn fork(&self) -> Pgrp {
        Pgrp {
            devtab: self.devtab.clone(),
            root: self.root.clone(),
            mounts: RwLock::new(self.mounts.read().clone()),
            dot: Mutex::new(self.dot()),
            nodevs: AtomicBool::new(self.nodevs.load(Ordering::Relaxed)),
        }
    }

    /// An empty namespace whose root is this one's current directory
    /// (`NEWNS`).
    pub fn newns(&self) -> Result<Arc<Pgrp>> {
        let mut root = self.namec(&self.dot(), "/")?;
        root.name = String::from("/");
        root.union = None;
        Ok(Arc::new(Pgrp {
            devtab: self.devtab.clone(),
            root: Arc::new(root),
            mounts: RwLock::new(Vec::new()),
            dot: Mutex::new(String::from("/")),
            nodevs: AtomicBool::new(self.nodevs.load(Ordering::Relaxed)),
        }))
    }

    /// Forbids attaching devices other than `#e #s #| #M` (`NODEVS`).
    pub fn set_nodevs(&self) {
        self.nodevs.store(true, Ordering::Relaxed);
    }

    /// `chdir`: `path` must be a directory.
    pub fn chdir(&self, path: &str) -> Result<()> {
        let c = self.namec(path, &self.dot())?;
        if !c.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        *self.dot.lock() = c.name.clone();
        Ok(())
    }

    /// `bind(new, old, flags)`: makes `new` visible at `old`.
    ///
    /// With `MBEFORE` or `MAFTER` the mount point becomes a union; when it is
    /// not yet mounted on, the original directory is its other member.
    pub fn bind(&self, new: &str, old: &str, flags: u32) -> Result<()> {
        let to = self.namec_unmounted(new)?;
        let from = self.namec_unmounted(old)?;
        if to.is_dir() != from.is_dir() {
            return Err(Error::new(error::EISMOUNT));
        }
        if flags & mount::MORDER != mount::MREPL && !from.is_dir() {
            return Err(Error::new("mount must be a directory to union"));
        }
        let key = from.key();
        let to = Member {
            chan: Arc::new(to),
            flags,
        };
        let mut mounts = self.mounts.write();
        let idx = match mounts.iter().position(|m| m.key == key) {
            Some(i) => i,
            None => {
                mounts.push(Mount {
                    key,
                    to: vec![Member {
                        chan: Arc::new(from),
                        flags: 0,
                    }],
                });
                mounts.len() - 1
            }
        };
        let m = &mut mounts[idx];
        match flags & mount::MORDER {
            mount::MBEFORE => m.to.insert(0, to),
            mount::MAFTER => m.to.push(to),
            _ => m.to = vec![to],
        }
        Ok(())
    }

    /// `unmount(name, old)`: removes `name` from the union at `old`, or
    /// everything mounted there when `name` is `None`.
    pub fn unmount(&self, name: Option<&str>, old: &str) -> Result<()> {
        let from = self.namec_unmounted(old)?;
        let key = from.key();
        let target = match name {
            Some(n) => Some(self.namec_unmounted(n)?.key()),
            None => None,
        };
        let mut mounts = self.mounts.write();
        let i = mounts
            .iter()
            .position(|m| m.key == key)
            .ok_or(Error::new(error::EUNMOUNT))?;
        match target {
            None => {
                mounts.remove(i);
            }
            Some(t) => {
                let m = &mut mounts[i];
                let j =
                    m.to.iter()
                        .position(|x| x.chan.key() == t)
                        .ok_or(Error::new(error::EUNMOUNT))?;
                m.to.remove(j);
                let only_original = m.to.len() == 1 && m.to[0].chan.key() == key;
                if m.to.is_empty() || only_original {
                    mounts.remove(i);
                }
            }
        }
        Ok(())
    }

    /// Union members mounted on `c`, if any.
    fn mounted(&self, c: &Chan) -> Option<Vec<Member>> {
        let key = c.key();
        self.mounts
            .read()
            .iter()
            .find(|m| m.key == key)
            .map(|m| m.to.clone())
    }

    /// Resolves a cleaned absolute or `#` path to an unopened Chan, crossing
    /// mount points. `dot` is used for relative paths.
    pub fn namec(&self, path: &str, dot: &str) -> Result<Chan> {
        self.resolve(path, dot, true)
    }

    fn namec_unmounted(&self, path: &str) -> Result<Chan> {
        self.resolve(path, &self.dot(), false)
    }

    /// Resolves the directory `path` would be created in and picks the union
    /// member to create in: the directory itself, or the first `MCREATE`
    /// member of a union. Returns that directory and the new file's name.
    pub fn create_target(&self, path: &str, dot: &str) -> Result<(Chan, String)> {
        let full = absolute(path, dot);
        let (parent, name) = match full.rsplit_once('/') {
            Some(("", n)) => (String::from("/"), n),
            Some((p, n)) => (String::from(p), n),
            None => return Err(Error::new(error::EFILENAME)),
        };
        if name.is_empty() || name == "." || name == ".." || split_device(&full).1.is_empty() {
            return Err(Error::new(error::EFILENAME));
        }
        let dir = self.namec(&parent, dot)?;
        if !dir.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        let target = match &dir.union {
            None => dir,
            Some(members) => {
                let m = members
                    .iter()
                    .find(|m| m.can_create())
                    .ok_or(Error::new(error::ENOCREATE))?;
                let mut c = m.chan.clone_unopened();
                c.name = dir.name.clone();
                c.union = None;
                c
            }
        };
        Ok((target, String::from(name)))
    }

    fn resolve(&self, path: &str, dot: &str, cross_last: bool) -> Result<Chan> {
        if path.is_empty() {
            return Err(Error::new(error::ENONEXIST));
        }
        let full = absolute(path, dot);
        let start = match split_device(&full) {
            (Some(dev), _) => {
                let mut chars = dev[1..].chars();
                let dc = chars.next().ok_or(Error::new(error::EFILENAME))?;
                if self.nodevs.load(Ordering::Relaxed) && !NODEVS_ALLOWED.contains(&dc) {
                    return Err(Error::new(error::ENOATTACH));
                }
                self.devtab.attach(dc, chars.as_str())?
            }
            (None, _) => self.root.clone_unopened(),
        };
        let prefix = match split_device(&full) {
            (Some(dev), _) => String::from(dev),
            (None, _) => String::new(),
        };
        let elems: Vec<&str> = elements(&full).collect();
        let mut cur = self.domount(start, cross_last || !elems.is_empty());
        let mut name = prefix;
        for (i, e) in elems.iter().enumerate() {
            let last = i + 1 == elems.len();
            let members: Vec<Arc<Chan>> = match &cur.union {
                Some(u) => u.iter().map(|m| m.chan.clone()).collect(),
                None => vec![],
            };
            let mut found = None;
            let mut err = Error::new(error::ENONEXIST);
            let candidates: Vec<&Chan> = if members.is_empty() {
                vec![&cur]
            } else {
                members.iter().map(|c| &**c).collect()
            };
            for cand in candidates {
                if !cand.is_dir() {
                    err = Error::new(error::ENOTDIR);
                    continue;
                }
                match cand.dev.walk(cand, e) {
                    Ok(w) => {
                        found = Some(Chan::new(
                            cand.dev.clone(),
                            cand.devno,
                            w.qid,
                            String::new(),
                            w.aux,
                        ));
                        break;
                    }
                    Err(e) => err = e,
                }
            }
            let mut next = found.ok_or(err)?;
            name.push('/');
            name.push_str(e);
            next.name = name.clone();
            cur = self.domount(next, cross_last || !last);
        }
        cur.name = if full.starts_with('#') || !full.starts_with('/') {
            full
        } else if name.is_empty() {
            String::from("/")
        } else {
            name
        };
        Ok(cur)
    }

    /// If `c` is a mount point (and `cross` is set), returns the first union
    /// member carrying the full union; otherwise returns `c`.
    fn domount(&self, c: Chan, cross: bool) -> Chan {
        if !cross {
            return c;
        }
        match self.mounted(&c) {
            Some(to) => {
                let mut first = to[0].chan.clone_unopened();
                first.name = c.name.clone();
                first.union = Some(to.into());
                first
            }
            None => c,
        }
    }
}

/// `path` made absolute against `dot` and cleaned; `#` paths stay as they
/// are.
pub fn absolute(path: &str, dot: &str) -> String {
    if path.starts_with('/') || path.starts_with('#') {
        cleanname(path)
    } else {
        let mut s = String::from(dot);
        s.push('/');
        s.push_str(path);
        cleanname(&s)
    }
}
