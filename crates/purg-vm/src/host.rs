//! Host functions. Each takes the prog context, the guest memory and the raw
//! WASM arguments. Errors are recorded as the prog's errstr; WASI functions
//! then return the mapped errno and `purgatorio` functions return `-1`.

use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::error;
use purg_core::Error;
use purg_styx::Dir;

use crate::mem::Mem;
use crate::{Ctx, HostResult, Stop};

/// The WASI preview1 subset, implemented over the namespace.
pub mod wasi {
    use super::*;
    use purg_abi::wasi::{self as w, errno};
    use purg_abi::{PREOPEN_DOT_FD, PREOPEN_ROOT_FD, mode};

    /// Runs `f`, turning an error into its errno and recording the errstr.
    fn call(ctx: &mut Ctx, f: impl FnOnce(&mut Ctx) -> Result<(), Error>) -> HostResult<i32> {
        Ok(match f(ctx) {
            Ok(()) => errno::SUCCESS,
            Err(e) => {
                ctx.prog.set_error(&e);
                purg_abi::errno_for(e.as_str())
            }
        })
    }

    fn strings_sizes(
        list: &[String],
        mem: &mut Mem<'_>,
        count: i32,
        size: i32,
    ) -> Result<(), Error> {
        let total: usize = list.iter().map(|s| s.len() + 1).sum();
        mem.put_u32(count, list.len() as u32)?;
        mem.put_u32(size, total as u32)?;
        Ok(())
    }

    fn strings_get(list: &[String], mem: &mut Mem<'_>, ptrs: i32, buf: i32) -> Result<(), Error> {
        let mut at = buf as u32;
        for (i, s) in list.iter().enumerate() {
            mem.put_u32(ptrs.wrapping_add(4 * i as i32), at)?;
            mem.write(at as i32, s.as_bytes())?;
            mem.put_u8(at.wrapping_add(s.len() as u32) as i32, 0)?;
            at = at.wrapping_add(s.len() as u32 + 1);
        }
        Ok(())
    }

    pub fn args_sizes_get(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        count: i32,
        size: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| strings_sizes(&c.argv, mem, count, size))
    }

    pub fn args_get(ctx: &mut Ctx, mem: &mut Mem<'_>, ptrs: i32, buf: i32) -> HostResult<i32> {
        call(ctx, |c| strings_get(&c.argv, mem, ptrs, buf))
    }

    pub fn environ_sizes_get(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        count: i32,
        size: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| strings_sizes(&c.env, mem, count, size))
    }

    pub fn environ_get(ctx: &mut Ctx, mem: &mut Mem<'_>, ptrs: i32, buf: i32) -> HostResult<i32> {
        call(ctx, |c| strings_get(&c.env, mem, ptrs, buf))
    }

    /// `(buf, len)` pairs of an iovec array.
    fn iovecs(mem: &Mem<'_>, iovs: i32, n: i32) -> Result<Vec<(i32, i32)>, Error> {
        (0..n)
            .map(|i| {
                let at = iovs.wrapping_add(8 * i);
                Ok((mem.u32(at)? as i32, mem.u32(at.wrapping_add(4))? as i32))
            })
            .collect()
    }

    pub fn fd_read(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        iovs: i32,
        n: i32,
        nread: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut total = 0usize;
            // One read per call: Inferno reads may be short (cons returns a
            // line), and a short read must not block for more.
            if let Some(&(buf, len)) = iovecs(mem, iovs, n)?.iter().find(|v| v.1 != 0) {
                total = c.prog.read(fd, mem.slice_mut(buf, len)?)?;
            }
            mem.put_u32(nread, total as u32)?;
            Ok(())
        })
    }

    pub fn fd_write(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        iovs: i32,
        n: i32,
        nwritten: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut total = 0usize;
            for (buf, len) in iovecs(mem, iovs, n)? {
                let data = mem.slice(buf, len)?;
                let w = c.prog.write(fd, data)?;
                total += w;
                if w < data.len() {
                    break;
                }
            }
            mem.put_u32(nwritten, total as u32)?;
            Ok(())
        })
    }

    pub fn fd_close(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.close(fd))
    }

    pub fn fd_seek(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        offset: i64,
        whence: i32,
        newoff: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let whence = u8::try_from(whence).map_err(|_| Error::new(error::EBADARG))?;
            let off = c.prog.seek(fd, offset, whence)?;
            mem.put_u64(newoff, off)?;
            Ok(())
        })
    }

    fn filetype(c: &purg_core::Chan) -> u8 {
        if c.is_dir() {
            w::FILETYPE_DIRECTORY
        } else if c.dev.dc() == 'c' {
            w::FILETYPE_CHARACTER_DEVICE
        } else {
            w::FILETYPE_REGULAR_FILE
        }
    }

    pub fn fd_fdstat_get(ctx: &mut Ctx, mem: &mut Mem<'_>, fd: i32, buf: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let ch = c.prog.chan(fd)?;
            mem.write(buf, &[0; 24])?;
            mem.put_u8(buf, filetype(&ch))?;
            mem.put_u16(buf.wrapping_add(2), 0)?;
            mem.put_u64(buf.wrapping_add(8), u64::MAX)?;
            mem.put_u64(buf.wrapping_add(16), u64::MAX)?;
            Ok(())
        })
    }

    fn preopen_name(fd: i32) -> Option<&'static str> {
        match fd {
            PREOPEN_ROOT_FD => Some("/"),
            PREOPEN_DOT_FD => Some("."),
            _ => None,
        }
    }

    pub fn fd_prestat_get(ctx: &mut Ctx, mem: &mut Mem<'_>, fd: i32, buf: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let name = preopen_name(fd).ok_or(Error::new(error::EBADFD))?;
            c.prog.chan(fd)?;
            mem.put_u32(buf, u32::from(w::PREOPENTYPE_DIR))?;
            mem.put_u32(buf.wrapping_add(4), name.len() as u32)?;
            Ok(())
        })
    }

    pub fn fd_prestat_dir_name(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        path: i32,
        len: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let name = preopen_name(fd).ok_or(Error::new(error::EBADFD))?;
            c.prog.chan(fd)?;
            let n = name.len().min(len as u32 as usize);
            mem.write(path, &name.as_bytes()[..n])?;
            Ok(())
        })
    }

    fn put_filestat(mem: &mut Mem<'_>, buf: i32, d: &Dir, ftype: u8) -> Result<(), Error> {
        let ns = |s: u32| u64::from(s) * 1_000_000_000;
        mem.write(buf, &[0; 64])?;
        mem.put_u64(buf, (u64::from(d.dtype) << 32) | u64::from(d.dev))?;
        mem.put_u64(buf.wrapping_add(8), d.qid.path)?;
        mem.put_u8(buf.wrapping_add(16), ftype)?;
        mem.put_u64(buf.wrapping_add(24), 1)?;
        mem.put_u64(buf.wrapping_add(32), d.length)?;
        mem.put_u64(buf.wrapping_add(40), ns(d.atime))?;
        mem.put_u64(buf.wrapping_add(48), ns(d.mtime))?;
        mem.put_u64(buf.wrapping_add(56), ns(d.mtime))?;
        Ok(())
    }

    pub fn fd_filestat_get(ctx: &mut Ctx, mem: &mut Mem<'_>, fd: i32, buf: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let ch = c.prog.chan(fd)?;
            let d = c.prog.fstat(fd)?;
            put_filestat(mem, buf, &d, filetype(&ch))
        })
    }

    /// Resolves `path` relative to the directory open on `dirfd`. The `.`
    /// preopen always means the prog's current dot.
    fn at_path(ctx: &Ctx, dirfd: i32, path: &str) -> Result<String, Error> {
        if path.starts_with('/') {
            return Ok(String::from(path));
        }
        let base = if dirfd == PREOPEN_DOT_FD {
            ctx.prog.dot()
        } else {
            let c = ctx.prog.chan(dirfd)?;
            if !c.is_dir() {
                return Err(Error::new(error::ENOTDIR));
            }
            c.name.clone()
        };
        Ok(alloc::format!("{base}/{path}"))
    }

    pub fn path_filestat_get(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dirfd: i32,
        _flags: i32,
        path: i32,
        plen: i32,
        buf: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = at_path(c, dirfd, mem.str(path, plen)?)?;
            let ch = c.prog.namec(&p)?;
            let d = c.prog.stat(&p)?;
            put_filestat(mem, buf, &d, filetype(&ch))
        })
    }

    pub fn fd_readdir(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: i32,
        len: i32,
        cookie: i64,
        used: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let entries = c.prog.dirents(fd)?;
            let len = len as u32 as usize;
            let mut out = Vec::with_capacity(len);
            for (i, d) in entries.iter().enumerate().skip(cookie as u64 as usize) {
                if out.len() >= len {
                    break;
                }
                let ftype = if d.qid.is_dir() {
                    w::FILETYPE_DIRECTORY
                } else {
                    w::FILETYPE_REGULAR_FILE
                };
                out.extend_from_slice(&(i as u64 + 1).to_le_bytes());
                out.extend_from_slice(&d.qid.path.to_le_bytes());
                out.extend_from_slice(&(d.name.len() as u32).to_le_bytes());
                out.extend_from_slice(&[ftype, 0, 0, 0]);
                out.extend_from_slice(d.name.as_bytes());
            }
            // A full buffer tells the caller to come back for more.
            out.truncate(len);
            mem.write(buf, &out)?;
            mem.put_u32(used, out.len() as u32)?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn path_open(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dirfd: i32,
        _dirflags: i32,
        path: i32,
        plen: i32,
        oflags: i32,
        rights: i64,
        _inheriting: i64,
        fdflags: i32,
        fd: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = at_path(c, dirfd, mem.str(path, plen)?)?;
            let oflags = oflags as u16;
            let rights = rights as u64;
            let write = rights & w::RIGHTS_FD_WRITE != 0;
            let read = rights & w::RIGHTS_FD_READ != 0;
            let access = match (read, write) {
                (true, true) => mode::ORDWR,
                (false, true) => mode::OWRITE,
                _ => mode::OREAD,
            };
            let trunc = if oflags & w::OFLAGS_TRUNC != 0 {
                mode::OTRUNC
            } else {
                0
            };
            let existing = c.prog.namec(&p);
            let newfd = match existing {
                Ok(_) if oflags & w::OFLAGS_EXCL != 0 && oflags & w::OFLAGS_CREAT != 0 => {
                    return Err(Error::new(error::EEXIST));
                }
                Ok(ch) => {
                    if oflags & w::OFLAGS_DIRECTORY != 0 && !ch.is_dir() {
                        return Err(Error::new(error::ENOTDIR));
                    }
                    let omode = if ch.is_dir() {
                        mode::OREAD
                    } else {
                        access | trunc
                    };
                    drop(ch);
                    c.prog.open(&p, omode)?
                }
                Err(e) if oflags & w::OFLAGS_CREAT == 0 || e.as_str() != error::ENONEXIST => {
                    return Err(e);
                }
                Err(_) => c.prog.create(&p, access, 0o666)?,
            };
            if fdflags as u16 & w::FDFLAGS_APPEND != 0 {
                c.prog.seek(newfd, 0, w::WHENCE_END)?;
            }
            mem.put_u32(fd, newfd as u32)?;
            Ok(())
        })
    }

    pub fn fd_pread(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        iovs: i32,
        n: i32,
        offset: i64,
        nread: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut total = 0usize;
            if let Some(&(buf, len)) = iovecs(mem, iovs, n)?.iter().find(|v| v.1 != 0) {
                total = c.prog.pread(fd, mem.slice_mut(buf, len)?, offset as u64)?;
            }
            mem.put_u32(nread, total as u32)?;
            Ok(())
        })
    }

    pub fn fd_pwrite(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        iovs: i32,
        n: i32,
        offset: i64,
        nwritten: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut total = 0usize;
            for (buf, len) in iovecs(mem, iovs, n)? {
                let data = mem.slice(buf, len)?;
                let w = c
                    .prog
                    .pwrite(fd, data, (offset as u64).wrapping_add(total as u64))?;
                total += w;
                if w < data.len() {
                    break;
                }
            }
            mem.put_u32(nwritten, total as u32)?;
            Ok(())
        })
    }

    pub fn fd_tell(ctx: &mut Ctx, mem: &mut Mem<'_>, fd: i32, off: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let o = c.prog.seek(fd, 0, w::WHENCE_CUR)?;
            mem.put_u64(off, o)?;
            Ok(())
        })
    }

    /// Inferno has no per-fd flags; only clearing them is accepted.
    pub fn fd_fdstat_set_flags(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        flags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog.chan(fd)?;
            if flags != 0 {
                return Err(Error::new(error::ENOTIMPL));
            }
            Ok(())
        })
    }

    pub fn fd_filestat_set_size(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        size: i64,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut d = Dir::null();
            d.length = size as u64;
            c.prog.fwstat(fd, &d)
        })
    }

    fn times_dir(c: &Ctx, mtim: i64, fst: i32) -> Dir {
        let mut d = Dir::null();
        let fst = fst as u16;
        if fst & w::FSTFLAGS_MTIM_NOW != 0 {
            d.mtime = (c.prog.platform().now_ns() / 1_000_000_000) as u32;
        } else if fst & w::FSTFLAGS_MTIM != 0 {
            d.mtime = (mtim as u64 / 1_000_000_000) as u32;
        }
        d
    }

    pub fn fd_filestat_set_times(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        _atim: i64,
        mtim: i64,
        fst: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = times_dir(c, mtim, fst);
            c.prog.fwstat(fd, &d)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn path_filestat_set_times(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dirfd: i32,
        _flags: i32,
        path: i32,
        plen: i32,
        _atim: i64,
        mtim: i64,
        fst: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = at_path(c, dirfd, mem.str(path, plen)?)?;
            let d = times_dir(c, mtim, fst);
            c.prog.wstat(&p, &d)
        })
    }

    /// `fd_sync` and `fd_datasync`: Inferno syncs with a null wstat, which
    /// devices without storage reject; that is not a failure to sync.
    pub fn fd_sync(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let _ = c.prog.fwstat(fd, &Dir::null());
            c.prog.chan(fd).map(drop)
        })
    }

    pub fn fd_renumber(ctx: &mut Ctx, _mem: &mut Mem<'_>, from: i32, to: i32) -> HostResult<i32> {
        call(ctx, |c| {
            if to < 0 {
                return Err(Error::new(error::EBADFD));
            }
            c.prog.dup(from, to)?;
            if from != to {
                c.prog.close(from)?;
            }
            Ok(())
        })
    }

    pub fn path_create_directory(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dirfd: i32,
        path: i32,
        plen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = at_path(c, dirfd, mem.str(path, plen)?)?;
            if c.prog.namec(&p).is_ok() {
                return Err(Error::new(error::EEXIST));
            }
            let fd = c.prog.create(&p, mode::OREAD, purg_styx::DMDIR | 0o777)?;
            c.prog.close(fd)
        })
    }

    fn remove_kind(
        ctx: &mut Ctx,
        mem: &Mem<'_>,
        dirfd: i32,
        path: i32,
        plen: i32,
        dir: bool,
    ) -> Result<(), Error> {
        let p = at_path(ctx, dirfd, mem.str(path, plen)?)?;
        let is_dir = ctx.prog.namec(&p)?.is_dir();
        match (dir, is_dir) {
            (true, false) => Err(Error::new(error::ENOTDIR)),
            (false, true) => Err(Error::new(error::EISDIR)),
            _ => ctx.prog.remove(&p),
        }
    }

    pub fn path_remove_directory(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dirfd: i32,
        path: i32,
        plen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| remove_kind(c, mem, dirfd, path, plen, true))
    }

    pub fn path_unlink_file(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dirfd: i32,
        path: i32,
        plen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| remove_kind(c, mem, dirfd, path, plen, false))
    }

    /// Inferno renames by `wstat` of the name, which stays in the same
    /// directory; anything else is `EXDEV`.
    #[allow(clippy::too_many_arguments)]
    pub fn path_rename(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        oldfd: i32,
        old: i32,
        olen: i32,
        newfd: i32,
        new: i32,
        nlen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let from = purg_core::names::cleanname(&at_path(c, oldfd, mem.str(old, olen)?)?);
            let to = purg_core::names::cleanname(&at_path(c, newfd, mem.str(new, nlen)?)?);
            let parent = |p: &str| p.rsplit_once('/').map(|(d, _)| String::from(d));
            if parent(&from) != parent(&to) {
                return Err(Error::new(error::EXDEV));
            }
            if from == to {
                return c.prog.namec(&from).map(drop);
            }
            if c.prog.namec(&to).is_ok() {
                c.prog.remove(&to)?;
            }
            let mut d = Dir::null();
            d.name = String::from(to.rsplit('/').next().unwrap_or(&to));
            c.prog.wstat(&from, &d)
        })
    }

    /// Inferno has no links.
    #[allow(clippy::too_many_arguments)]
    pub fn path_readlink(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        _dirfd: i32,
        _path: i32,
        _plen: i32,
        _buf: i32,
        _blen: i32,
        _used: i32,
    ) -> HostResult<i32> {
        call(ctx, |_| Err(Error::new(error::ENOTIMPL)))
    }

    /// Sleeps for clock subscriptions; fd subscriptions are reported ready
    /// at once, since every Inferno read and write may block anyway.
    pub fn poll_oneoff(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        subs: i32,
        events: i32,
        n: i32,
        nevents: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            const SUB: i32 = 48;
            const EVENT: i32 = 32;
            let mut ready = Vec::new();
            let mut sleep_ns: Option<u64> = None;
            for i in 0..n {
                let at = subs.wrapping_add(i.wrapping_mul(SUB));
                let userdata = mem.slice(at, 8)?;
                let userdata = u64::from_le_bytes(userdata.try_into().expect("8 bytes"));
                let tag = mem.slice(at.wrapping_add(8), 1)?[0];
                match tag {
                    w::EVENTTYPE_CLOCK => {
                        let b = mem.slice(at.wrapping_add(24), 8)?;
                        let timeout = u64::from_le_bytes(b.try_into().expect("8 bytes"));
                        let f = mem.slice(at.wrapping_add(40), 2)?;
                        let flags = u16::from_le_bytes([f[0], f[1]]);
                        let rel = if flags & w::SUBCLOCKFLAGS_ABSTIME != 0 {
                            timeout.saturating_sub(c.prog.platform().monotonic_ns())
                        } else {
                            timeout
                        };
                        if sleep_ns.is_none_or(|s| rel < s) {
                            sleep_ns = Some(rel);
                        }
                        ready.push((userdata, tag, rel));
                    }
                    _ => ready.push((userdata, tag, 0)),
                }
            }
            let fd_ready = ready.iter().any(|r| r.1 != w::EVENTTYPE_CLOCK);
            let mut out = 0;
            if !fd_ready && let Some(ns) = sleep_ns {
                purg_platform::sync::Rendez::new().tsleep(|| false, ns);
            }
            for (userdata, tag, rel) in ready {
                if tag == w::EVENTTYPE_CLOCK && (fd_ready || Some(rel) != sleep_ns) {
                    continue;
                }
                let at = events.wrapping_add(out * EVENT);
                mem.write(at, &[0; EVENT as usize])?;
                mem.write(at, &userdata.to_le_bytes())?;
                mem.put_u8(at.wrapping_add(10), tag)?;
                out += 1;
            }
            mem.put_u32(nevents, out as u32)?;
            Ok(())
        })
    }

    pub fn sched_yield(_ctx: &mut Ctx, _mem: &mut Mem<'_>) -> HostResult<i32> {
        Ok(errno::SUCCESS)
    }

    pub fn clock_time_get(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        id: i32,
        _precision: i64,
        time: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = c.prog.platform();
            let now = match id as u32 {
                w::CLOCKID_REALTIME => p.now_ns(),
                w::CLOCKID_MONOTONIC => p.monotonic_ns(),
                _ => return Err(Error::new(error::EBADARG)),
            };
            mem.put_u64(time, now)?;
            Ok(())
        })
    }

    pub fn random_get(ctx: &mut Ctx, mem: &mut Mem<'_>, buf: i32, len: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let out = mem.slice_mut(buf, len)?;
            c.prog.platform().entropy(out);
            Ok(())
        })
    }

    /// `proc_exit(0)` is status `""`; `proc_exit(n)` is `"exit n"`.
    pub fn proc_exit(_ctx: &mut Ctx, _mem: &mut Mem<'_>, code: i32) -> HostResult<()> {
        Err(Stop::Exit(if code == 0 {
            String::new()
        } else {
            alloc::format!("exit {}", code as u32)
        }))
    }
}

/// The `purgatorio` extension module.
pub mod purgatorio {
    use super::*;

    /// Runs `f`, turning an error into `-1` and recording the errstr.
    fn call(ctx: &mut Ctx, f: impl FnOnce(&mut Ctx) -> Result<i32, Error>) -> HostResult<i32> {
        Ok(match f(ctx) {
            Ok(n) => n,
            Err(e) => {
                ctx.prog.set_error(&e);
                -1
            }
        })
    }

    /// Copies the errstr into `buf` (truncated to `len`), returning its length.
    pub fn errstr(ctx: &mut Ctx, mem: &mut Mem<'_>, buf: i32, len: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let s = c.prog.errstr.as_bytes();
            let n = s.len().min(len as u32 as usize);
            mem.write(buf, &s[..n])?;
            Ok(n as i32)
        })
    }

    pub fn werrstr(ctx: &mut Ctx, mem: &mut Mem<'_>, ptr: i32, len: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let s = String::from(mem.str(ptr, len)?);
            c.prog.errstr = s;
            Ok(0)
        })
    }

    pub fn exits(_ctx: &mut Ctx, mem: &mut Mem<'_>, ptr: i32, len: i32) -> HostResult<()> {
        let status = match mem.str(ptr, len) {
            Ok(s) => String::from(s),
            Err(e) => String::from(e.as_str()),
        };
        Err(Stop::Exit(status))
    }

    fn put_dir(mem: &mut Mem<'_>, buf: i32, len: i32, d: &Dir) -> Result<i32, Error> {
        let packed = d.pack().map_err(|_| Error::new(error::EIO))?;
        if packed.len() > len as u32 as usize {
            return Err(Error::new(error::ESHORTSTAT));
        }
        mem.write(buf, &packed)?;
        Ok(packed.len() as i32)
    }

    pub fn stat(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: i32,
        plen: i32,
        buf: i32,
        len: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = c.prog.stat(mem.str(path, plen)?)?;
            put_dir(mem, buf, len, &d)
        })
    }

    pub fn fstat(ctx: &mut Ctx, mem: &mut Mem<'_>, fd: i32, buf: i32, len: i32) -> HostResult<i32> {
        call(ctx, |c| {
            let d = c.prog.fstat(fd)?;
            put_dir(mem, buf, len, &d)
        })
    }

    pub fn dirread(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: i32,
        len: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let out = mem.slice_mut(buf, len)?;
            Ok(c.prog.dirread(fd, out)? as i32)
        })
    }

    fn put_str(mem: &mut Mem<'_>, buf: i32, len: i32, v: &str) -> Result<i32, Error> {
        if v.len() > len as u32 as usize {
            return Err(Error::new(error::ESHORTSTAT));
        }
        mem.write(buf, v.as_bytes())?;
        Ok(v.len() as i32)
    }

    fn get_dir(mem: &Mem<'_>, ptr: i32, len: i32) -> Result<Dir, Error> {
        Dir::unpack(mem.slice(ptr, len)?)
            .map(|(d, _)| d)
            .map_err(|_| Error::new(error::EBADARG))
    }

    fn get_i32s(mem: &Mem<'_>, ptr: i32, n: i32) -> Result<Vec<i32>, Error> {
        if n < 0 {
            return Err(Error::new(error::EBADARG));
        }
        (0..n)
            .map(|i| Ok(mem.u32(ptr.wrapping_add(4 * i))? as i32))
            .collect()
    }

    fn omode(m: i32) -> Result<u8, Error> {
        u8::try_from(m).map_err(|_| Error::new(error::EBADARG))
    }

    pub fn open(ctx: &mut Ctx, mem: &mut Mem<'_>, path: i32, plen: i32, m: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.open(mem.str(path, plen)?, omode(m)?))
    }

    pub fn create(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: i32,
        plen: i32,
        m: i32,
        perm: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog.create(mem.str(path, plen)?, omode(m)?, perm as u32)
        })
    }

    pub fn wstat(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: i32,
        plen: i32,
        dir: i32,
        dlen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = get_dir(mem, dir, dlen)?;
            c.prog.wstat(mem.str(path, plen)?, &d).map(|()| 0)
        })
    }

    pub fn fwstat(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        dir: i32,
        dlen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = get_dir(mem, dir, dlen)?;
            c.prog.fwstat(fd, &d).map(|()| 0)
        })
    }

    pub fn remove(ctx: &mut Ctx, mem: &mut Mem<'_>, path: i32, plen: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.remove(mem.str(path, plen)?).map(|()| 0))
    }

    pub fn fd2path(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: i32,
        len: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = if fd == purg_abi::PREOPEN_DOT_FD {
                c.prog.dot()
            } else {
                c.prog.fd2path(fd)?
            };
            put_str(mem, buf, len, &p)
        })
    }

    pub fn chdir(ctx: &mut Ctx, mem: &mut Mem<'_>, path: i32, plen: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.chdir(mem.str(path, plen)?).map(|()| 0))
    }

    pub fn dup(ctx: &mut Ctx, _mem: &mut Mem<'_>, old: i32, new: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.dup(old, new))
    }

    pub fn pipe(ctx: &mut Ctx, mem: &mut Mem<'_>, fds: i32) -> HostResult<i32> {
        call(ctx, |c| {
            mem.slice(fds, 8)?;
            let [a, b] = c.prog.pipe()?;
            mem.put_u32(fds, a as u32)?;
            mem.put_u32(fds.wrapping_add(4), b as u32)?;
            Ok(0)
        })
    }

    /// No device limits I/O sizes yet, so every fd reports 0 ("no limit").
    pub fn iounit(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.chan(fd).map(|_| 0))
    }

    pub fn bind(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        name: i32,
        nlen: i32,
        old: i32,
        olen: i32,
        flags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog
                .bind(mem.str(name, nlen)?, mem.str(old, olen)?, flags as u32)
                .map(|()| 0)
        })
    }

    pub fn unmount(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        name: i32,
        nlen: i32,
        old: i32,
        olen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let name = if nlen == 0 {
                None
            } else {
                Some(mem.str(name, nlen)?)
            };
            c.prog.unmount(name, mem.str(old, olen)?).map(|()| 0)
        })
    }

    pub fn pctl(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        flags: i32,
        movefd: i32,
        n: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let keep = get_i32s(mem, movefd, n)?;
            let pid = c.prog.pctl(flags as u32, &keep)?;
            if flags as u32 & purg_abi::pctl::NEWFD != 0 {
                crate::init_preopens(&mut c.prog)?;
            }
            Ok(pid as i32)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_prog(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: i32,
        plen: i32,
        argv: i32,
        alen: i32,
        flags: i32,
        fdmap: i32,
        n: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let path = crate::command_path(mem.str(path, plen)?);
            let args: Vec<String> = mem
                .str(argv, alen)?
                .split_terminator('\0')
                .map(String::from)
                .collect();
            let pairs = get_i32s(
                mem,
                fdmap,
                n.checked_mul(2).ok_or(Error::new(error::EBADARG))?,
            )?;
            let map: Vec<(i32, i32)> = pairs.chunks(2).map(|p| (p[0], p[1])).collect();
            let engine = c.engine.clone();
            let pid = crate::spawn(&c.prog, &engine, &path, args, flags as u32, &map)?;
            Ok(pid as i32)
        })
    }
}
