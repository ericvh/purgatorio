//! The Sys file operations (Inferno's `sysfile.c`), as methods on [`Prog`].

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_styx::{DMDIR, Dir};

use crate::chan::Chan;
use crate::error::{Error, Result};
use crate::pgrp::absolute;
use crate::prog::Prog;

impl Prog {
    /// Resolves `path` in this prog's namespace without opening it.
    pub fn namec(&self, path: &str) -> Result<Chan> {
        self.pgrp.namec(path, &self.dot())
    }

    /// Resolves and opens `path`, returning the open Chan.
    pub fn open_chan(&self, path: &str, omode: u8) -> Result<Chan> {
        let c = self.namec(path)?;
        open(c, omode)
    }

    /// `sys->open`: returns the lowest free fd.
    pub fn open(&mut self, path: &str, omode: u8) -> Result<i32> {
        let c = self.open_chan(path, omode)?;
        self.fgrp.alloc(Arc::new(c))
    }

    /// Creates `path` (truncating an existing file) and opens it.
    pub fn create_chan(&self, path: &str, omode: u8, perm: u32) -> Result<Chan> {
        // As in Inferno, an existing name is opened and truncated rather than
        // created, so `>` works on device files such as /dev/null.
        if let Ok(c) = self.namec(path) {
            if perm & DMDIR != 0 {
                return Err(Error::new(error::EEXIST));
            }
            return open(c, omode | mode::OTRUNC);
        }
        let (dir, name) = self.pgrp.create_target(path, &self.dot())?;
        if perm & DMDIR != 0 && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EISDIR));
        }
        let w = dir.dev.create(&dir, &name, perm)?;
        let mut c = Chan::new(dir.dev.clone(), dir.devno, w.qid, String::new(), w.aux);
        c.name = absolute(path, &self.dot());
        open(c, omode & !mode::OTRUNC)
    }

    /// `sys->create`: returns the lowest free fd.
    pub fn create(&mut self, path: &str, omode: u8, perm: u32) -> Result<i32> {
        let c = self.create_chan(path, omode, perm)?;
        self.fgrp.alloc(Arc::new(c))
    }

    pub fn remove(&self, path: &str) -> Result<()> {
        let c = self.namec(path)?;
        c.dev.remove(&c)
    }

    pub fn wstat(&self, path: &str, d: &Dir) -> Result<()> {
        let c = self.namec(path)?;
        c.dev.wstat(&c, d)
    }

    pub fn fwstat(&self, fd: i32, d: &Dir) -> Result<()> {
        let c = self.fgrp.get(fd)?;
        c.dev.wstat(&c, d)
    }

    pub fn close(&mut self, fd: i32) -> Result<()> {
        self.fgrp.close(fd)
    }

    pub fn chan(&self, fd: i32) -> Result<Arc<Chan>> {
        self.fgrp.get(fd)
    }

    /// `sys->read`, advancing the fd's offset. Reading a directory returns
    /// whole packed `Dir`s.
    pub fn read(&mut self, fd: i32, buf: &mut [u8]) -> Result<usize> {
        let c = self.fgrp.get(fd)?;
        check_access(&c, mode::OREAD)?;
        // As in Inferno's kread, the offset lock is not held across the
        // device read, which may block (a pipe, the console).
        let off = *c.offset.lock();
        let n = if c.is_dir() {
            dir_read(&c, buf, off)?
        } else {
            c.dev.read(&c, buf, off)?
        };
        *c.offset.lock() += n as u64;
        Ok(n)
    }

    /// `sys->pread`: read at `offset` without moving the fd's offset.
    pub fn pread(&mut self, fd: i32, buf: &mut [u8], offset: u64) -> Result<usize> {
        let c = self.fgrp.get(fd)?;
        check_access(&c, mode::OREAD)?;
        if c.is_dir() {
            return Err(Error::new(error::EISDIR));
        }
        c.dev.read(&c, buf, offset)
    }

    pub fn write(&mut self, fd: i32, buf: &[u8]) -> Result<usize> {
        let c = self.fgrp.get(fd)?;
        check_access(&c, mode::OWRITE)?;
        if c.is_dir() {
            return Err(Error::new(error::EISDIR));
        }
        let off = *c.offset.lock();
        let n = c.dev.write(&c, buf, off)?;
        *c.offset.lock() += n as u64;
        Ok(n)
    }

    /// `sys->pwrite`: write at `offset` without moving the fd's offset.
    pub fn pwrite(&mut self, fd: i32, buf: &[u8], offset: u64) -> Result<usize> {
        let c = self.fgrp.get(fd)?;
        check_access(&c, mode::OWRITE)?;
        if c.is_dir() {
            return Err(Error::new(error::EISDIR));
        }
        c.dev.write(&c, buf, offset)
    }

    /// `sys->seek`. Whence is 0 (start), 1 (current) or 2 (end).
    pub fn seek(&mut self, fd: i32, offset: i64, whence: u8) -> Result<u64> {
        let c = self.fgrp.get(fd)?;
        let mut off = c.offset.lock();
        let base: i64 = match whence {
            0 => 0,
            1 => *off as i64,
            2 => c.dev.stat(&c)?.length as i64,
            _ => return Err(Error::new(error::EBADARG)),
        };
        let new = base.checked_add(offset).filter(|&n| n >= 0);
        let new = new.ok_or(Error::new(error::EBADARG))? as u64;
        if c.is_dir() && new != 0 {
            return Err(Error::new(error::EISDIR));
        }
        *off = new;
        Ok(new)
    }

    pub fn stat(&self, path: &str) -> Result<Dir> {
        let c = self.namec(path)?;
        stat_named(&c)
    }

    pub fn fstat(&self, fd: i32) -> Result<Dir> {
        stat_named(&*self.fgrp.get(fd)?)
    }

    /// `sys->dirread`: the next batch of packed entries, at most `max` bytes.
    pub fn dirread(&mut self, fd: i32, buf: &mut [u8]) -> Result<usize> {
        if !self.fgrp.get(fd)?.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        self.read(fd, buf)
    }

    /// Every entry of the directory open on `fd`, across union members.
    pub fn dirents(&self, fd: i32) -> Result<Vec<Dir>> {
        let c = self.fgrp.get(fd)?;
        if !c.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        union_dirents(&c)
    }

    /// `sys->bind`.
    pub fn bind(&self, new: &str, old: &str, flags: u32) -> Result<()> {
        let dot = self.dot();
        self.pgrp
            .bind(&absolute(new, &dot), &absolute(old, &dot), flags)
    }

    /// `sys->unmount`; `name = None` removes everything mounted on `old`.
    pub fn unmount(&self, name: Option<&str>, old: &str) -> Result<()> {
        let dot = self.dot();
        let name = name.map(|n| absolute(n, &dot));
        self.pgrp.unmount(name.as_deref(), &absolute(old, &dot))
    }

    pub fn chdir(&self, path: &str) -> Result<()> {
        self.pgrp.chdir(path)
    }

    /// `sys->fd2path`: the name the fd's file was opened by.
    pub fn fd2path(&self, fd: i32) -> Result<String> {
        Ok(self.fgrp.get(fd)?.name.clone())
    }

    /// `sys->dup(old, new)`: `new = -1` picks the lowest free fd.
    pub fn dup(&mut self, old: i32, new: i32) -> Result<i32> {
        let c = self.fgrp.get(old)?;
        if new < 0 {
            self.fgrp.alloc(c)
        } else {
            self.fgrp.install(new, c)?;
            Ok(new)
        }
    }

    /// `sys->mount`. `afd >= 0` (a separate auth fd) is not used; the
    /// conversation on `fd` carries `Tauth` unless `MNOAUTH` is set.
    pub fn mount(&self, fd: i32, afd: i32, old: &str, flags: u32, aname: &str) -> Result<()> {
        if afd >= 0 {
            return Err(Error::new(error::ENOTIMPL));
        }
        let noauth = flags & purg_abi::mount::MNOAUTH != 0;
        let flags = flags & !purg_abi::mount::MNOAUTH;
        let fd = self.fgrp.get(fd)?;
        let pgrp = self.pgrp.clone();
        let user = String::from(self.kernel.procs.user());
        let root = self
            .kernel
            .mnt
            .mount(fd, aname, &user, noauth, || client_secret(&pgrp, &user))?;
        let old = absolute(old, &self.dot());
        self.pgrp.bind_chan(root, &old, flags)
    }

    /// `sys->export`: serve `dir` on `fd` until the client closes.
    /// `MNOAUTH` serves the tree even when it contains a public key.
    pub fn export(&self, fd: i32, dir: &str, flags: u32) -> Result<()> {
        let fd = self.fgrp.get(fd)?;
        let dir = absolute(dir, &self.dot());
        crate::export::serve(
            self.pgrp.clone(),
            &dir,
            &fd,
            self.kernel.platform.as_ref(),
            flags & purg_abi::mount::MNOAUTH != 0,
        )
    }

    /// `sys->file2chan`. When `dir` is not on `#s`, the file is created in
    /// `#s` and bound over `dir/name`, creating an empty file there first
    /// when the name does not exist.
    pub fn file2chan(&mut self, dir: &str, name: &str) -> Result<(i32, i32)> {
        if name.is_empty() || name.contains('/') {
            return Err(Error::new(error::EBADARG));
        }
        let dir = absolute(dir, &self.dot());
        let parent = self.namec(&dir)?;
        if !parent.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        if parent.dev.dc() == 's' {
            return self.kernel.srv.add(&self.proc, name);
        }
        let walk = self.kernel.srv.fresh_name();
        let handles = self.kernel.srv.add(&self.proc, &walk)?;
        let target = if dir == "/" {
            alloc::format!("/{name}")
        } else {
            alloc::format!("{dir}/{name}")
        };
        if self.namec(&target).is_err() {
            let fd = self.create(&target, mode::OWRITE, 0o666)?;
            self.close(fd)?;
        }
        self.bind(
            &alloc::format!("#s/{walk}"),
            &target,
            purg_abi::mount::MREPL,
        )?;
        Ok(handles)
    }

    /// `sys->fversion`: exchange a 9P version on `fd`. Returns the version
    /// string the server agreed to.
    pub fn fversion(&self, fd: i32, msize: u32, ver: &str) -> Result<String> {
        use purg_styx::{Fcall, Msg};
        let c = self.fgrp.get(fd)?;
        let bytes = Msg {
            tag: 0,
            fcall: Fcall::Tversion {
                msize,
                version: ver,
            },
        }
        .encode()
        .map_err(|_| Error::new(error::EIO))?;
        crate::rw::write_full(&c, &bytes)?;
        let buf = crate::rw::read_msg(&c, msize.max(purg_styx::IOHDRSZ))?;
        match Msg::decode(&buf, msize.max(purg_styx::IOHDRSZ)) {
            Ok(Msg {
                fcall: Fcall::Rversion { version, .. },
                ..
            }) => Ok(String::from(version)),
            Ok(Msg {
                fcall: Fcall::Rerror { ename },
                ..
            }) => Err(Error::from(String::from(ename))),
            _ => Err(Error::new(error::EIO)),
        }
    }

    /// `sys->iounit`.
    pub fn iounit_of(&self, fd: i32) -> Result<u32> {
        let c = self.fgrp.get(fd)?;
        Ok(c.dev.iounit(&c))
    }

    /// `sys->pipe`: two fds open on the ends of a new `#|` pipe.
    pub fn pipe(&mut self) -> Result<[i32; 2]> {
        let dir = self.namec("#|")?;
        let end = |name: &str| -> Result<Chan> {
            let w = dir.dev.walk(&dir, name)?;
            let c = Chan::new(
                dir.dev.clone(),
                dir.devno,
                w.qid,
                alloc::format!("#|/{name}"),
                w.aux,
            );
            open(c, mode::ORDWR)
        };
        let (d0, d1) = (end("data")?, end("data1")?);
        let fd0 = self.fgrp.alloc(Arc::new(d0))?;
        match self.fgrp.alloc(Arc::new(d1)) {
            Ok(fd1) => Ok([fd0, fd1]),
            Err(e) => {
                let _ = self.fgrp.close(fd0);
                Err(e)
            }
        }
    }

    pub fn chan_new(&self, cap: u32) -> Result<i32> {
        self.proc
            .kgrp
            .alloc(Arc::new(crate::kchan::KChan::new(cap as usize)))
    }

    pub fn chan_close(&self, h: i32) -> Result<()> {
        let c = self.proc.kgrp.close(h)?;
        c.close();
        Ok(())
    }

    pub fn chan_send(&self, h: i32, bytes: &[u8], handles: &[i32]) -> Result<()> {
        let mut caps = Vec::new();
        for &id in handles {
            caps.push(crate::kchan::Cap::Chan(self.proc.kgrp.get(id)?));
        }
        let ch = self.proc.kgrp.get(h)?;
        ch.send(crate::kchan::KMsg {
            bytes: bytes.to_vec(),
            caps,
        })?;
        for &id in handles {
            let _ = self.proc.kgrp.close(id);
        }
        Ok(())
    }
}

/// The arm that completed, its bytes, and any handles it received.
pub type AltDone = (usize, Vec<u8>, Vec<i32>);

impl Prog {
    /// One `alt` arm. On a receive, `got` is filled when that arm completes.
    pub fn chan_alt(
        &self,
        arms: &[(i32, bool, Vec<u8>, Vec<i32>)],
        block: bool,
    ) -> Result<Option<AltDone>> {
        let mut ops = Vec::new();
        for (handle, send, bytes, caps) in arms {
            let chan = self.proc.kgrp.get(*handle)?;
            let send = if *send {
                let mut moved = Vec::new();
                for &id in caps {
                    moved.push(crate::kchan::Cap::Chan(self.proc.kgrp.get(id)?));
                }
                Some(crate::kchan::KMsg {
                    bytes: bytes.clone(),
                    caps: moved,
                })
            } else {
                None
            };
            ops.push(crate::kchan::AltOp {
                chan,
                send,
                got: None,
            });
        }
        let Some(i) = crate::kchan::alt(&mut ops, block)? else {
            return Ok(None);
        };
        if arms[i].1 {
            for &id in &arms[i].3 {
                let _ = self.proc.kgrp.close(id);
            }
            return Ok(Some((i, Vec::new(), Vec::new())));
        }
        let msg = ops[i].got.take().ok_or(Error::new(error::EIO))?;
        let mut handles = Vec::new();
        for cap in msg.caps {
            if let crate::kchan::Cap::Chan(c) = cap {
                handles.push(self.proc.kgrp.alloc(c)?);
            }
        }
        Ok(Some((i, msg.bytes, handles)))
    }

    /// `purgatorio.wait`. `ops` is `(kind, id)` with kinds 0 fd-read, 1
    /// fd-write, 2 chan-recv, 3 chan-send. `timeout_ms < 0` waits forever.
    pub fn wait_ops(&self, ops: &[(i32, i32)], timeout_ms: i32) -> Result<Option<usize>> {
        let rendez = purg_platform::sync::Rendez::new();
        for &(kind, id) in ops {
            match kind {
                0 | 1 => {
                    if let Ok(c) = self.fgrp.get(id) {
                        c.dev.watch(&c, kind == 1, &rendez);
                    }
                }
                2 | 3 => {
                    if let Ok(c) = self.proc.kgrp.get(id) {
                        c.watch(&rendez);
                    }
                }
                _ => return Err(Error::new(error::EBADARG)),
            }
        }
        let ready = || {
            ops.iter().any(|&(kind, id)| match kind {
                0 | 1 => self
                    .fgrp
                    .get(id)
                    .map(|c| c.dev.ready(&c, kind == 1))
                    .unwrap_or(true),
                2 => self.proc.kgrp.get(id).map(|c| c.can_recv()).unwrap_or(true),
                3 => self.proc.kgrp.get(id).map(|c| c.can_send()).unwrap_or(true),
                _ => true,
            })
        };
        let hit = crate::block::park_timeout(&rendez, ready, timeout_ms);
        for &(kind, id) in ops {
            match kind {
                0 | 1 => {
                    if let Ok(c) = self.fgrp.get(id) {
                        c.dev.unwatch(&c, &rendez);
                    }
                }
                2 | 3 => {
                    if let Ok(c) = self.proc.kgrp.get(id) {
                        c.unwatch(&rendez);
                    }
                }
                _ => {}
            }
        }
        if !hit? {
            return Ok(None);
        }
        Ok(ops.iter().position(|&(kind, id)| match kind {
            0 | 1 => self
                .fgrp
                .get(id)
                .map(|c| c.dev.ready(&c, kind == 1))
                .unwrap_or(true),
            2 => self.proc.kgrp.get(id).map(|c| c.can_recv()).unwrap_or(true),
            3 => self.proc.kgrp.get(id).map(|c| c.can_send()).unwrap_or(true),
            _ => true,
        }))
    }

    /// Receives one message. Returns the bytes and the new handles the
    /// capabilities were installed as. `Ok(None)` is a closed channel.
    pub fn chan_recv(&self, h: i32, max: usize) -> Result<Option<(Vec<u8>, Vec<i32>)>> {
        let ch = self.proc.kgrp.get(h)?;
        let Some(msg) = ch.recv()? else {
            return Ok(None);
        };
        let mut handles = Vec::new();
        for cap in msg.caps {
            let id = match cap {
                crate::kchan::Cap::Chan(c) => self.proc.kgrp.alloc(c)?,
                crate::kchan::Cap::Fd(fd) => self.fgrp.alloc(fd)?,
            };
            handles.push(id);
        }
        let bytes = if msg.bytes.len() > max {
            msg.bytes[..max].to_vec()
        } else {
            msg.bytes
        };
        Ok(Some((bytes, handles)))
    }
}

/// The caller's signing seed: `/usr/$user/key`, then the shared test fixture.
fn client_secret(pgrp: &crate::pgrp::Pgrp, user: &str) -> Result<[u8; crate::auth::SEED]> {
    let mut paths = Vec::new();
    if !user.is_empty() && user != "." && user != ".." && !user.contains('/') {
        paths.push(alloc::format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for path in paths {
        if let Some(key) = crate::export::read_key(pgrp, &path) {
            return Ok(key);
        }
    }
    Err(Error::new(crate::auth::NO_KEY))
}

/// Opens an unopened Chan in `omode`.
pub(crate) fn open(mut c: Chan, omode: u8) -> Result<Chan> {
    let access = omode & mode::ACCESS;
    if c.is_dir() && (access != mode::OREAD || omode & mode::OTRUNC != 0) {
        return Err(Error::new(error::EISDIR));
    }
    let dev = c.dev.clone();
    dev.open(&mut c, omode)?;
    c.mode = omode;
    c.open = true;
    Ok(c)
}

fn check_access(c: &Chan, want: u8) -> Result<()> {
    let have = c.mode & mode::ACCESS;
    let ok = match want {
        mode::OREAD => matches!(have, mode::OREAD | mode::ORDWR | mode::OEXEC),
        _ => matches!(have, mode::OWRITE | mode::ORDWR),
    };
    if c.open && ok {
        Ok(())
    } else {
        Err(Error::new(error::EBADUSEFD))
    }
}

fn stat_named(c: &Chan) -> Result<Dir> {
    let mut d = c.dev.stat(c)?;
    if c.name == "/" {
        d.name = String::from("/");
    }
    Ok(d)
}

fn union_dirents(c: &Chan) -> Result<Vec<Dir>> {
    match &c.union {
        Some(members) => {
            let mut all = Vec::new();
            for m in members.iter() {
                all.extend(m.chan.dev.dirread(&m.chan)?);
            }
            Ok(all)
        }
        None => c.dev.dirread(c),
    }
}

/// Serves a directory read at byte `offset` from a snapshot taken when the
/// read starts at offset 0. Only whole entries are returned.
pub(crate) fn dir_read(c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut snap = c.dirbuf.lock();
    if offset == 0 {
        snap.clear();
        for d in union_dirents(c)? {
            d.pack_into(&mut snap).map_err(|_| Error::new(error::EIO))?;
        }
    }
    let start = usize::try_from(offset).map_err(|_| Error::new(error::EBADARG))?;
    let rest = snap.get(start..).unwrap_or(&[]);
    let mut n = 0;
    while n < rest.len() {
        let len = u16::from_le_bytes([rest[n], rest[n + 1]]) as usize + 2;
        if n + len > buf.len() {
            break;
        }
        n += len;
    }
    if n == 0 && !rest.is_empty() {
        return Err(Error::new(error::ESHORTSTAT));
    }
    buf[..n].copy_from_slice(&rest[..n]);
    Ok(n)
}
