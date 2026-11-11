//! Namespaces: Inferno's `Pgrp` and mount table, plus name resolution
//! (`namec`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use purg_abi::{error, mount};
use purg_platform::sync::{Mutex, RwLock};

use crate::chan::{Chan, Member};
use crate::dev::DevTab;
use crate::error::{Error, Result};
use crate::names::{cleanname, elements, split_device};

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

/// The current directory: Inferno's `dot`, a held Chan and not only a
/// name.
///
/// A name in or below the current directory is walked from this Chan, so
/// it leads to the same files whatever has since been bound over, or
/// unmounted from, the names that led here. Holding the Chan also keeps
/// the directory referenced: on a file server, its fid stays live.
///
/// Names are still cleaned lexically first, so `..` out of the current
/// directory is resolved by name, from the root.
#[derive(Clone)]
struct Dot {
    /// The cleaned absolute name `chdir` was given.
    name: String,
    /// The directory `chdir` found. `None` for the namespace's root.
    chan: Option<Arc<Chan>>,
    /// Mount-table identity of the directory `chan` was mounted on when
    /// `chdir` crossed a mount point to reach it.
    mtpt: Option<(char, u32, u64)>,
}

impl Dot {
    fn root() -> Dot {
        Dot {
            name: String::from("/"),
            chan: None,
            mtpt: None,
        }
    }
}

/// A namespace: a root, a mount table and the current directory.
pub struct Pgrp {
    devtab: Arc<DevTab>,
    root: Arc<Chan>,
    mounts: RwLock<Vec<Mount>>,
    dot: Mutex<Dot>,
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
            dot: Mutex::new(Dot::root()),
            nodevs: AtomicBool::new(false),
        })
    }

    pub fn devtab(&self) -> &Arc<DevTab> {
        &self.devtab
    }

    /// The current directory's name.
    pub fn dot(&self) -> String {
        self.dot.lock().name.clone()
    }

    /// A copy of this namespace (`FORKNS`).
    pub fn fork(&self) -> Pgrp {
        Pgrp {
            devtab: self.devtab.clone(),
            root: self.root.clone(),
            mounts: RwLock::new(self.mounts.read().clone()),
            dot: Mutex::new(self.dot.lock().clone()),
            nodevs: AtomicBool::new(self.nodevs.load(Ordering::Relaxed)),
        }
    }

    /// An empty namespace whose root is this one's current directory
    /// (`NEWNS`).
    pub fn newns(&self) -> Result<Arc<Pgrp>> {
        let mut root = self.namec(".", &self.dot())?;
        root.name = String::from("/");
        root.union = None;
        Ok(Arc::new(Pgrp {
            devtab: self.devtab.clone(),
            root: Arc::new(root),
            mounts: RwLock::new(Vec::new()),
            dot: Mutex::new(Dot::root()),
            nodevs: AtomicBool::new(self.nodevs.load(Ordering::Relaxed)),
        }))
    }

    /// An empty namespace on the same devices, rooted at `#/`, for a prog
    /// that is exiting: it replaces the prog's namespace so that what
    /// that held is released before the prog leaves the prog table.
    pub fn exited(&self) -> Arc<Pgrp> {
        let root = match self.devtab.attach('/', "") {
            Ok(mut c) => {
                c.name = String::from("/");
                Arc::new(c)
            }
            Err(_) => self.root.clone(),
        };
        Arc::new(Pgrp {
            devtab: self.devtab.clone(),
            root,
            mounts: RwLock::new(Vec::new()),
            dot: Mutex::new(Dot::root()),
            nodevs: AtomicBool::new(true),
        })
    }

    /// Forbids attaching devices other than `#e #s #| #M` (`NODEVS`).
    pub fn set_nodevs(&self) {
        self.nodevs.store(true, Ordering::Relaxed);
    }

    /// `chdir`: `path` must be a directory. The Chan it resolves to is
    /// held as the current directory until the next `chdir`.
    pub fn chdir(&self, path: &str) -> Result<()> {
        let dot = self.dot();
        // `cd .` stays where it is, whatever the name leads to now.
        let relative = !path.starts_with('/') && !path.starts_with('#');
        if relative && absolute(path, &dot) == dot {
            return Ok(());
        }
        let under = self.resolve(path, &dot, false)?;
        let mtpt = under.key();
        let mut c = self.domount(under, true);
        if !c.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        // The union is looked up again at each use: it may grow.
        let mounted = c.union.take().is_some();
        let new = if c.name == "/" {
            Dot::root()
        } else {
            Dot {
                name: c.name.clone(),
                chan: Some(Arc::new(c)),
                mtpt: mounted.then_some(mtpt),
            }
        };
        // The old Chan is dropped after the lock: for a `#M` file that
        // clunks its fid, which waits for the file server.
        let old = core::mem::replace(&mut *self.dot.lock(), new);
        drop(old);
        Ok(())
    }

    /// Where to start walking `full`, a name given relative to `dot`, when
    /// it is in or below the current directory: that directory's held
    /// Chan, its name, and the rest of `full`.
    ///
    /// If the directory was reached through a mount point that still has
    /// it as its first member, the Chan carries the union as it is now.
    /// Otherwise it is the directory alone, even when something has been
    /// mounted on its name since: Inferno's `dot` does not follow a mount
    /// made after the `chdir`.
    fn start_at_dot<'a>(&self, full: &'a str, dot: &str) -> Option<(Chan, String, &'a str)> {
        let held = self.dot.lock().clone();
        let chan = held.chan.as_ref()?;
        if held.name != dot {
            return None;
        }
        let rest = match full.strip_prefix(held.name.as_str())? {
            "" => "",
            r => r.strip_prefix('/')?,
        };
        let mut cur = chan.clone_unopened();
        cur.union = None;
        if let Some(key) = held.mtpt {
            let live = self
                .mounts
                .read()
                .iter()
                .find(|m| m.from.key() == key)
                .filter(|m| m.to.first().is_some_and(|f| f.chan.key() == cur.key()))
                .map(|m| m.to.clone());
            if let Some(to) = live {
                cur.union = Some(to.into());
            }
        }
        Some((cur, held.name.clone(), rest))
    }

    /// `bind(new, old, flags)`: makes `new` visible at `old`.
    ///
    /// With `MBEFORE` or `MAFTER` the mount point becomes a union; when it is
    /// not yet mounted on, the original directory is its other member.
    ///
    /// `new` is resolved through the mount table, as Inferno's `bind` does:
    /// when it is a mount point, what is mounted there is bound, and when
    /// that is a union every member is.
    pub fn bind(&self, new: &str, old: &str, flags: u32) -> Result<()> {
        let to = self.namec(new, &self.dot())?;
        self.bind_chan(to, old, flags)
    }

    /// Binds an existing Chan at `old`. `mount` uses this for a `#M` root.
    ///
    /// When `to` is a union, its other members follow it at `old`, in
    /// order, with the same `MBEFORE` or `MAFTER` (and without `MCREATE`).
    pub fn bind_chan(&self, mut to: Chan, old: &str, flags: u32) -> Result<()> {
        let from = self.namec_unmounted(old)?;
        if to.is_dir() != from.is_dir() {
            return Err(Error::new(error::EISMOUNT));
        }
        if flags & mount::MORDER != mount::MREPL && !from.is_dir() {
            return Err(Error::new("mount must be a directory to union"));
        }
        let rest = to.union.take();
        let mut to = vec![Member {
            chan: Arc::new(to),
            flags,
            id: 0,
        }];
        if let Some(rest) = rest {
            let order = match flags & mount::MORDER {
                mount::MBEFORE => mount::MBEFORE,
                _ => mount::MAFTER,
            };
            to.extend(rest.iter().skip(1).map(|m| Member {
                chan: m.chan.clone(),
                flags: order,
                id: 0,
            }));
        }
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
                        id: next_mount_id(),
                    }]
                };
                mounts.push(Mount { from, to });
                mounts.len() - 1
            }
        };
        // Ids are taken under the lock, in the order `cmount` takes them:
        // the new member, then the union members that follow it.
        for member in &mut to {
            member.id = next_mount_id();
        }
        let m = &mut mounts[idx];
        match flags & mount::MORDER {
            mount::MBEFORE => drop(m.to.splice(0..0, to)),
            mount::MAFTER => m.to.extend(to),
            _ => released = core::mem::replace(&mut m.to, to),
        }
        drop(mounts);
        drop(released);
        Ok(())
    }

    /// `unmount(name, old)`: removes `name` from the union at `old`, or
    /// everything mounted there when `name` is `None`.
    ///
    /// `name` is resolved as `bind` resolved it, through the mount table,
    /// and the member is found as Inferno's `eqchan` does: same device and
    /// qid path. The directory `name` is mounted on also matches, for a
    /// member bound before anything was mounted there.
    pub fn unmount(&self, name: Option<&str>, old: &str) -> Result<()> {
        let from = self.namec_unmounted(old)?;
        let target = match name {
            Some(n) => Some([
                self.namec(n, &self.dot())?.key(),
                self.namec_unmounted(n)?.key(),
            ]),
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
                let j = t
                    .iter()
                    .find_map(|k| m.to.iter().position(|x| x.chan.key() == *k))
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
        // The parent is named in full here, but a name given relative to
        // the current directory is still walked from it.
        let relative = !path.starts_with('/') && !path.starts_with('#');
        let dir = self.resolve_full(&parent, dot, true, relative)?;
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
        let relative = !path.starts_with('/') && !path.starts_with('#');
        self.resolve_full(&absolute(path, dot), dot, cross_last, relative)
    }

    /// Resolves the cleaned absolute name `full`. When it was given
    /// `relative` to `dot` and does not leave the current directory, the
    /// walk starts at the Chan held for that directory; otherwise at the
    /// root or the device the name starts with.
    fn resolve_full(
        &self,
        full: &str,
        dot: &str,
        cross_last: bool,
        relative: bool,
    ) -> Result<Chan> {
        let held = if relative {
            self.start_at_dot(full, dot)
        } else {
            None
        };
        let (mut cur, mut name, elems): (Chan, String, Vec<&str>) = match held {
            Some((cur, name, rest)) => {
                let elems = rest.split('/').filter(|e| !e.is_empty()).collect();
                (cur, name, elems)
            }
            None => {
                let (start, prefix) = match split_device(full) {
                    (Some(dev), _) => {
                        let mut chars = dev[1..].chars();
                        let dc = chars.next().ok_or(Error::new(error::EFILENAME))?;
                        if self.nodevs.load(Ordering::Relaxed) && !NODEVS_ALLOWED.contains(&dc) {
                            return Err(Error::new(error::ENOATTACH));
                        }
                        (self.devtab.attach(dc, chars.as_str())?, String::from(dev))
                    }
                    (None, _) => (self.root.clone_unopened(), String::new()),
                };
                let elems: Vec<&str> = elements(full).collect();
                let cur = self.domount(start, cross_last || !elems.is_empty());
                (cur, prefix, elems)
            }
        };
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
            String::from(full)
        } else if name.is_empty() {
            String::from("/")
        } else {
            name
        };
        Ok(cur)
    }

    /// The namespace as `bind` and `mount` lines, then `cd`, in the raw
    /// form of Inferno's `/prog/n/ns`: one line for each bind or mount, in
    /// the order they were made (Inferno's `mountid`), so that running the
    /// lines again builds the same unions. The underlying directory of a
    /// union is not listed.
    ///
    /// Each line is `bind flags new old`, or `mount flags source old spec`,
    /// with nothing quoted. The flags are `-a`, `-b`, `-c` and their
    /// combinations, and are empty for a replacing bind, which leaves two
    /// spaces after the command; an empty spec leaves a trailing space.
    /// The `ns` command quotes and tidies these lines.
    ///
    /// A member is shown as a mount only when it is the root a mount
    /// attached, as Inferno decides from the Chan's name being `#M`: its
    /// source is the name of the file the 9P conversation is on (Inferno's
    /// `mchan`), and the attach name follows. A directory reached through
    /// a mounted tree and bound elsewhere is a bind of its name.
    pub fn namespace(&self) -> String {
        let mounts = self.mounts.read();
        let mut lines: Vec<(&Member, &Mount)> = mounts
            .iter()
            .flat_map(|m| {
                m.to.iter()
                    .filter(|member| !m.is_original(member))
                    .map(move |member| (member, m))
            })
            .collect();
        lines.sort_by_key(|&(member, _)| member.id);
        let mut out = String::new();
        for (member, m) in lines {
            let c = &member.chan;
            let (fl, old) = (flag_text(member.flags), &m.from.name);
            let cmd = match c.dev.mount_source(c) {
                Some(src) => {
                    let spec = c.dev.aname(c);
                    alloc::format!("mount {fl} {src} {old} {spec}\n")
                }
                None => alloc::format!("bind {fl} {} {old}\n", c.name),
            };
            out.push_str(&cmd);
        }
        drop(mounts);
        out.push_str(&alloc::format!("cd {}\n", self.dot()));
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

/// The next id for a mount-table member: Inferno's `mountid`, shared by
/// every namespace so that a fork keeps its parent's order.
fn next_mount_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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
