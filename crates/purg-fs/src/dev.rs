//! `#K`: a mounted purgfs as a kernel device (the `dev` feature).
//!
//! The kernel binds it where the file system belongs (`/n/disk`); like any
//! device it is then served over Styx to whoever imports or mounts that
//! part of the namespace. Each Chan carries the entry's [`Loc`], which goes
//! stale (`file does not exist`) once the file is removed. One lock covers
//! the file system: operations run one at a time, each in the calling
//! prog's thread, so a prog killed while it waits for the disk stops at
//! once (reads are interruptible; writes finish, see `docs/fs.md`).
//!
//! Progs act as the platform's user, or, given the process table
//! ([`DevFs::with_users`]), each as its own user (`/dev/user`); the mode
//! bits apply to files other users own. `DMEXCL` files admit one
//! open at a time (`exclusive use file already open`), and `ORCLOSE`
//! removes the file at its last close.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_core::{Chan, Dev, Error, Platform, Procs, Result, Walked};
use purg_platform::sync::Mutex;
use purg_styx::{Dir, QTAPPEND, QTDIR, QTEXCL, QTFILE, Qid};

use crate::layout::{DMAPPEND, DMEXCL};
use crate::{Dentry, Disk, Fs, Loc, Report, Usage, W, WStat, perm_ok};

/// The error for a second open of a `DMEXCL` file, as Inferno's kfs.
pub const EEXCL: &str = "exclusive use file already open";

/// The device letter.
pub const DC: char = 'K';

fn e(err: crate::Error) -> Error {
    Error::from(err)
}

/// A mounted file system as a device.
pub struct DevFs<D: Disk + Send> {
    fs: Mutex<Fs<D>>,
    user: String,
    /// Where the calling prog's user is found, when progs act as their
    /// own users.
    procs: Option<Arc<Procs>>,
    platform: Arc<dyn Platform>,
    /// Qid paths of the `DMEXCL` files that are open.
    excl: Mutex<BTreeSet<u64>>,
}

/// Per-open state: what close has to undo.
struct Opened {
    excl: bool,
    rclose: bool,
}

impl<D: Disk + Send> DevFs<D> {
    /// Serves `fs`, acting as `platform`'s user.
    pub fn new(fs: Fs<D>, platform: Arc<dyn Platform>) -> DevFs<D> {
        DevFs {
            user: platform.user(),
            procs: None,
            fs: Mutex::new(fs),
            platform,
            excl: Mutex::new(BTreeSet::new()),
        }
    }

    /// Makes each prog act as its own user, found in `procs`.
    pub fn with_users(mut self, procs: Arc<Procs>) -> DevFs<D> {
        self.procs = Some(procs);
        self
    }

    /// The user the calling prog acts as.
    fn user(&self) -> String {
        match &self.procs {
            Some(p) => p.current_user(),
            None => self.user.clone(),
        }
    }

    fn fs(&self) -> purg_platform::sync::MutexGuard<'_, Fs<D>> {
        let mut fs = self.fs.lock();
        fs.set_now((self.platform.now_ns() / 1_000_000_000) as u32);
        fs
    }

    /// Checks the whole file system ([`Fs::check`]).
    pub fn check(&self) -> Report {
        self.fs.lock().check()
    }

    pub fn usage(&self) -> Usage {
        self.fs.lock().usage()
    }

    /// Flushes the disk.
    pub fn sync(&self) -> Result<()> {
        self.fs.lock().sync().map_err(e)
    }

    fn loc(c: &Chan) -> Result<Loc> {
        c.aux::<Loc>().copied().ok_or(Error::new(error::EIO))
    }

    fn walked(loc: Loc, d: &Dentry) -> Walked {
        Walked {
            qid: qid(d),
            aux: Some(Arc::new(loc)),
        }
    }
}

fn qid(d: &Dentry) -> Qid {
    let mut qtype = if d.is_dir() { QTDIR } else { QTFILE };
    if d.mode & DMAPPEND != 0 {
        qtype |= QTAPPEND;
    }
    if d.mode & DMEXCL != 0 {
        qtype |= QTEXCL;
    }
    Qid {
        path: d.qpath,
        vers: d.qvers,
        qtype,
    }
}

fn dir(d: &Dentry) -> Dir {
    Dir {
        dtype: DC as u16,
        dev: 0,
        qid: qid(d),
        mode: d.mode,
        atime: d.atime,
        mtime: d.mtime,
        length: if d.is_dir() { 0 } else { d.length },
        name: d.name.clone(),
        uid: d.uid.clone(),
        gid: d.gid.clone(),
        muid: d.muid.clone(),
    }
}

impl<D: Disk + Send + 'static> Dev for DevFs<D> {
    fn dc(&self) -> char {
        DC
    }

    fn name(&self) -> &'static str {
        "purgfs"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        let mut fs = self.fs();
        let root = fs.root();
        let d = fs.dentry(root.me).map_err(e)?;
        Ok(Self::walked(root, &d))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let user = self.user();
        let mut fs = self.fs();
        let loc = fs.walk(Self::loc(c)?, name, &user).map_err(e)?;
        let d = fs.dentry(loc.me).map_err(e)?;
        Ok(Self::walked(loc, &d))
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let d = self.fs().dentry(Self::loc(c)?.me).map_err(e)?;
        Ok(dir(&d))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let user = self.user();
        let loc = Self::loc(c)?;
        let mut fs = self.fs();
        let d = fs.open_check(loc.me, &user, omode).map_err(e)?;
        let rclose = omode & mode::ORCLOSE != 0;
        if rclose {
            let parent = fs.dentry(loc.parent).map_err(e)?;
            if loc.me.block == 0 || !perm_ok(&parent, &user, W) {
                return Err(Error::new(error::EPERM));
            }
        }
        let excl = d.mode & DMEXCL != 0;
        if excl && !self.excl.lock().insert(d.qpath) {
            return Err(Error::from(EEXCL));
        }
        if omode & mode::OTRUNC != 0
            && !d.is_dir()
            && let Err(err) = fs.truncate(loc.me, 0)
        {
            if excl {
                self.excl.lock().remove(&d.qpath);
            }
            return Err(e(err));
        }
        let _ = fs.touch(loc.me);
        if let Ok(d) = fs.dentry(loc.me) {
            c.qid = qid(&d);
        }
        *c.state.lock() = Some(alloc::boxed::Box::new(Opened { excl, rclose }));
        Ok(())
    }

    fn close(&self, c: &Chan) {
        let Ok(loc) = Self::loc(c) else {
            return;
        };
        let opened = c.state.lock().take();
        let Some(o) = opened.and_then(|s| s.downcast::<Opened>().ok()) else {
            return;
        };
        if o.excl {
            self.excl.lock().remove(&loc.me.path);
        }
        if o.rclose {
            let user = self.user();
            let _ = self.fs().remove(loc, &user);
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.fs().read(Self::loc(c)?.me, offset, buf).map_err(e)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let entries = self.fs().readdir(Self::loc(c)?).map_err(e)?;
        Ok(entries.iter().map(|(_, d)| dir(d)).collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let user = self.user();
        self.fs()
            .write(Self::loc(c)?.me, offset, buf, &user)
            .map_err(e)
    }

    fn create(&self, dir: &Chan, name: &str, perm: u32) -> Result<Walked> {
        let user = self.user();
        let mut fs = self.fs();
        let loc = fs.create(Self::loc(dir)?, name, perm, &user).map_err(e)?;
        let d = fs.dentry(loc.me).map_err(e)?;
        Ok(Self::walked(loc, &d))
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        let user = self.user();
        self.fs().remove(Self::loc(c)?, &user).map_err(e)
    }

    fn wstat(&self, c: &Chan, d: &Dir) -> Result<()> {
        let user = self.user();
        let loc = Self::loc(c)?;
        let mut fs = self.fs();
        let cur = fs.dentry(loc.me).map_err(e)?;
        // Inferno: the type, device and qid cannot change.
        if (d.dtype != u16::MAX && d.dtype != DC as u16)
            || (d.dev != u32::MAX && d.dev != 0)
            || (d.qid.path != u64::MAX && d.qid.path != cur.qpath)
        {
            return Err(Error::new(error::EPERM));
        }
        let text = |s: &str| (!s.is_empty()).then(|| String::from(s));
        let w = WStat {
            name: text(&d.name),
            uid: text(&d.uid),
            gid: text(&d.gid),
            mode: (d.mode != u32::MAX).then_some(d.mode),
            length: (d.length != u64::MAX).then_some(d.length),
            mtime: (d.mtime != u32::MAX).then_some(d.mtime),
            atime: (d.atime != u32::MAX).then_some(d.atime),
        };
        fs.wstat(loc, &w, &user).map_err(e)
    }
}
