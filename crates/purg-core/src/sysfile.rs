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
fn dir_read(c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
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
