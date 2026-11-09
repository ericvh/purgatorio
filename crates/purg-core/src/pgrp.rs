//! Namespaces: Inferno's `Pgrp` and mount table, plus name resolution
//! (`namec`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use purg_abi::{error, mount};
use purg_platform::sync::RwLock;

use crate::chan::Chan;
use crate::dev::DevTab;
use crate::error::{Error, Result};
use crate::names::{cleanname, elements, split_device};

struct Mount {
    /// Identity of the mounted-on Chan.
    key: (char, u32, u64),
    /// Union members in search order.
    to: Vec<Arc<Chan>>,
}

/// A namespace: a root and a mount table.
pub struct Pgrp {
    devtab: Arc<DevTab>,
    root: Arc<Chan>,
    mounts: RwLock<Vec<Mount>>,
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
        })
    }

    pub fn devtab(&self) -> &Arc<DevTab> {
        &self.devtab
    }

    /// `bind(new, old, flags)`: makes `new` visible at `old`.
    ///
    /// With `MBEFORE` or `MAFTER` the mount point becomes a union; when it is
    /// not yet mounted on, the original directory is its other member.
    pub fn bind(&self, new: &str, old: &str, flags: u32) -> Result<()> {
        let to = self.namec_unmounted(new)?;
        let from = self.namec_unmounted(old)?;
        if to.is_dir() != from.is_dir() {
            return Err(Error::new("inconsistent mount"));
        }
        if flags & mount::MORDER != mount::MREPL && !from.is_dir() {
            return Err(Error::new("mount must be a directory to union"));
        }
        let key = from.key();
        let to = Arc::new(to);
        let mut mounts = self.mounts.write();
        let idx = match mounts.iter().position(|m| m.key == key) {
            Some(i) => i,
            None => {
                mounts.push(Mount {
                    key,
                    to: vec![Arc::new(from)],
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

    /// Union members mounted on `c`, if any.
    fn mounted(&self, c: &Chan) -> Option<Vec<Arc<Chan>>> {
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
        self.resolve(path, "/", false)
    }

    fn resolve(&self, path: &str, dot: &str, cross_last: bool) -> Result<Chan> {
        if path.is_empty() {
            return Err(Error::new(error::ENONEXIST));
        }
        let full = if path.starts_with('/') || path.starts_with('#') {
            cleanname(path)
        } else {
            let mut s = String::from(dot);
            s.push('/');
            s.push_str(path);
            cleanname(&s)
        };
        let start = match split_device(&full) {
            (Some(dev), _) => {
                let mut chars = dev[1..].chars();
                let dc = chars.next().ok_or(Error::new(error::EFILENAME))?;
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
                Some(u) => u.iter().cloned().collect(),
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
                let mut first = to[0].clone_unopened();
                first.name = c.name.clone();
                first.union = Some(to.into());
                first
            }
            None => c,
        }
    }
}
