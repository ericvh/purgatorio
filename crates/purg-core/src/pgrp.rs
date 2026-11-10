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
use crate::names::{cleanname, elements, sh_quote, split_device};

/// Devices a `NODEVS` namespace may still attach (Inferno's list).
const NODEVS_ALLOWED: &[char] = &['e', 's', '|', 'M', '¤'];

/// One entry of the mount table, as Inferno's `Mhead`.
///
/// The entry holds the Chan it was mounted on, so the file stays referenced
/// (for a `#M` file, its fid stays live) as long as the mount does. A file
/// server may forget a file nobody refers to and give its qid to another
/// file; holding the Chan stops that from moving or aliasing the mount.
///
/// Lookups match a Chan against `from` as Inferno's `findmount` does: same
/// device type, device instance and qid path, ignoring the qid version.
#[derive(Clone)]
struct Mount {
    /// The mounted-on Chan.
    from: Arc<Chan>,
    /// Union members in search order. For a union made with `MBEFORE` or
    /// `MAFTER` on a directory not yet mounted on, one member is `from`
    /// itself.
    to: Vec<Member>,
}

impl Mount {
    fn matches(&self, c: &Chan) -> bool {
        self.from.key() == c.key()
    }

    /// Whether `m` is the original directory of the union, not a bind.
    fn is_original(&self, m: &Member) -> bool {
        Arc::ptr_eq(&m.chan, &self.from)
    }
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
        self.bind_chan(to, old, flags)
    }

    /// Binds an existing Chan at `old`. `mount` uses this for a `#M` root.
    pub fn bind_chan(&self, to: Chan, old: &str, flags: u32) -> Result<()> {
        let from = self.namec_unmounted(old)?;
        if to.is_dir() != from.is_dir() {
            return Err(Error::new(error::EISMOUNT));
        }
        if flags & mount::MORDER != mount::MREPL && !from.is_dir() {
            return Err(Error::new("mount must be a directory to union"));
        }
        let to = Member {
            chan: Arc::new(to),
            flags,
        };
        // Chans released by this bind are dropped after the lock: dropping a
        // `#M` Chan clunks its fid, which waits for the file server.
        let mut released: Vec<Member> = Vec::new();
        let mut mounts = self.mounts.write();
        let idx = match mounts.iter().position(|m| m.matches(&from)) {
            Some(i) => i,
            None => {
                let from = Arc::new(from);
                let to = if flags & mount::MORDER == mount::MREPL {
                    Vec::new()
                } else {
                    vec![Member {
                        chan: from.clone(),
                        flags: 0,
                    }]
                };
                mounts.push(Mount { from, to });
                mounts.len() - 1
            }
        };
        let m = &mut mounts[idx];
        match flags & mount::MORDER {
            mount::MBEFORE => m.to.insert(0, to),
            mount::MAFTER => m.to.push(to),
            _ => released = core::mem::replace(&mut m.to, vec![to]),
        }
        drop(mounts);
        drop(released);
        Ok(())
    }

    /// `unmount(name, old)`: removes `name` from the union at `old`, or
    /// everything mounted there when `name` is `None`.
    pub fn unmount(&self, name: Option<&str>, old: &str) -> Result<()> {
        let from = self.namec_unmounted(old)?;
        let target = match name {
            Some(n) => Some(self.namec_unmounted(n)?.key()),
            None => None,
        };
        let mut mounts = self.mounts.write();
        let i = mounts
            .iter()
            .position(|m| m.matches(&from))
            .ok_or(Error::new(error::EUNMOUNT))?;
        // Dropped after the lock, as in `bind_chan`.
        let released: (Option<Mount>, Option<Member>) = match target {
            None => (Some(mounts.remove(i)), None),
            Some(t) => {
                let m = &mut mounts[i];
                let j =
                    m.to.iter()
                        .position(|x| x.chan.key() == t)
                        .ok_or(Error::new(error::EUNMOUNT))?;
                let gone = m.to.remove(j);
                let only_original = m.to.len() == 1 && m.is_original(&m.to[0]);
                if m.to.is_empty() || only_original {
                    (Some(mounts.remove(i)), Some(gone))
                } else {
                    (None, Some(gone))
                }
            }
        };
        drop(mounts);
        drop(released);
        Ok(())
    }

    /// Union members mounted on `c`, if any.
    fn mounted(&self, c: &Chan) -> Option<Vec<Member>> {
        self.mounts
            .read()
            .iter()
            .find(|m| m.matches(c))
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

    /// The namespace as `bind` and `mount` commands, then `cd`, in the order
    /// the unions were created. The underlying directory of a union is not
    /// listed. Flags are `-a`, `-b`, `-c` and combinations; replace has none.
    pub fn namespace(&self) -> String {
        let mounts = self.mounts.read();
        let mut out = String::new();
        for m in mounts.iter() {
            for member in &m.to {
                if m.is_original(member) {
                    continue;
                }
                let new = sh_quote(&member.chan.name);
                let old = sh_quote(&m.from.name);
                let fl = flag_text(member.flags);
                let cmd = if member.chan.dev.dc() == 'M' {
                    let spec = sh_quote(&member.chan.dev.aname(&member.chan));
                    if fl.is_empty() {
                        alloc::format!("mount {new} {old} {spec}\n")
                    } else {
                        alloc::format!("mount {fl} {new} {old} {spec}\n")
                    }
                } else if fl.is_empty() {
                    alloc::format!("bind {new} {old}\n")
                } else {
                    alloc::format!("bind {fl} {new} {old}\n")
                };
                out.push_str(&cmd);
            }
        }
        out.push_str(&alloc::format!("cd {}\n", sh_quote(&self.dot())));
        out
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

fn flag_text(flags: u32) -> String {
    let mut s = String::new();
    match flags & mount::MORDER {
        mount::MAFTER => s.push('a'),
        mount::MBEFORE => s.push('b'),
        _ => {}
    }
    if flags & mount::MCREATE != 0 {
        s.push('c');
    }
    if s.is_empty() {
        String::new()
    } else {
        alloc::format!("-{s}")
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
