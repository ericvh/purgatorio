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

/// WASI preview1, implemented over the namespace. Calls Inferno cannot
/// express (`path_link`, `path_symlink`) return `NOTSUP`.
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

    /// Append is applied at `path_open`. The other flags, including
    /// non-blocking, are accepted and ignored.
    pub fn fd_fdstat_set_flags(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        flags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog.chan(fd)?;
            if flags as u16 & !w::FDFLAGS_KNOWN != 0 {
                return Err(Error::new(error::EBADARG));
            }
            Ok(())
        })
    }

    pub fn fd_advise(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        _offset: i64,
        _len: i64,
        advice: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog.chan(fd)?;
            if !(0..=5).contains(&advice) {
                return Err(Error::new(error::EBADARG));
            }
            Ok(())
        })
    }

    /// Grows the file so it covers `offset + len`. A shorter file is left
    /// as it is.
    pub fn fd_allocate(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        offset: i64,
        len: i64,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            if offset < 0 || len < 0 {
                return Err(Error::new(error::EBADARG));
            }
            let end = offset.checked_add(len).ok_or(Error::new(error::EBADARG))?;
            let cur = c.prog.fstat(fd)?;
            if cur.qid.is_dir() {
                return Err(Error::new(error::EISDIR));
            }
            if cur.length < end as u64 {
                let mut d = Dir::null();
                d.length = end as u64;
                c.prog.fwstat(fd, &d)?;
            }
            Ok(())
        })
    }

    /// Rights are not tracked. The fd only has to be open.
    pub fn fd_fdstat_set_rights(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        _base: i64,
        _inheriting: i64,
    ) -> HostResult<i32> {
        call(ctx, |c| c.prog.chan(fd).map(drop))
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

    /// Inferno has no hard links. A missing source is `NOENT`; an existing
    /// one is `NOTSUP`.
    #[allow(clippy::too_many_arguments)]
    pub fn path_link(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        oldfd: i32,
        _oldflags: i32,
        old: i32,
        olen: i32,
        newfd: i32,
        new: i32,
        nlen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let from = at_path(c, oldfd, mem.str(old, olen)?)?;
            let _to = at_path(c, newfd, mem.str(new, nlen)?)?;
            c.prog.namec(&from)?;
            Err(Error::new(error::ENOTIMPL))
        })
    }

    /// Inferno has no symlinks.
    #[allow(clippy::too_many_arguments)]
    pub fn path_symlink(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        old: i32,
        olen: i32,
        fd: i32,
        new: i32,
        nlen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let _target = mem.str(old, olen)?;
            let _to = at_path(c, fd, mem.str(new, nlen)?)?;
            Err(Error::new(error::ENOTIMPL))
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

    /// One `poll_oneoff` subscription. `fd` is meaningful for the fd tags.
    struct Sub {
        userdata: u64,
        tag: u8,
        fd: i32,
        /// Nanoseconds until a clock subscription, already made relative.
        rel: u64,
    }

    fn sub_ready(prog: &purg_core::Prog, sub: &Sub) -> bool {
        if sub.tag == w::EVENTTYPE_CLOCK {
            return false;
        }
        match prog.chan(sub.fd) {
            Ok(ch) => ch.dev.ready(&ch, sub.tag == w::EVENTTYPE_FD_WRITE),
            Err(_) => true,
        }
    }

    /// Waits for the soonest clock, or for an fd whose device is not ready.
    /// File descriptors that are ready now are reported without sleeping.
    /// A zero-length poll returns success and no events.
    pub fn poll_oneoff(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        subs: i32,
        events: i32,
        n: i32,
        nevents: i32,
    ) -> HostResult<i32> {
        // A prog killed in a plain sleep stops here, with the status the
        // backends give a prog killed while computing. It could not act
        // on `INTR`: wasi-libc turns every failure of a sleep into
        // `ENOTSUP`, which Rust's `thread::sleep` answers with a panic.
        let mut killed_asleep = false;
        let errno = call(ctx, |c| {
            const SUB: i32 = 48;
            const EVENT: i32 = 32;
            if !(0..=128).contains(&n) {
                return Err(Error::new(error::EBADARG));
            }
            if n == 0 {
                mem.put_u32(nevents, 0)?;
                return Ok(());
            }
            let mut items = Vec::with_capacity(n as usize);
            let mut soonest: Option<u64> = None;
            for i in 0..n {
                let at = subs.wrapping_add(i.wrapping_mul(SUB));
                let userdata = mem.slice(at, 8)?;
                let userdata = u64::from_le_bytes(userdata.try_into().expect("8 bytes"));
                let tag = mem.slice(at.wrapping_add(8), 1)?[0];
                let (fd, rel) = match tag {
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
                        soonest = Some(soonest.map_or(rel, |s| s.min(rel)));
                        (0, rel)
                    }
                    w::EVENTTYPE_FD_READ | w::EVENTTYPE_FD_WRITE => {
                        (mem.u32(at.wrapping_add(16))? as i32, 0)
                    }
                    _ => return Err(Error::new(error::EBADARG)),
                };
                items.push(Sub {
                    userdata,
                    tag,
                    fd,
                    rel,
                });
            }
            let any_fd = items.iter().any(|s| s.tag != w::EVENTTYPE_CLOCK);
            let ready_now = items.iter().any(|s| sub_ready(&c.prog, s));
            if !ready_now && any_fd {
                let rendez = purg_platform::sync::Rendez::new();
                let mut watched = Vec::new();
                for s in &items {
                    if s.tag == w::EVENTTYPE_CLOCK {
                        continue;
                    }
                    if let Ok(ch) = c.prog.chan(s.fd) {
                        ch.dev.watch(&ch, s.tag == w::EVENTTYPE_FD_WRITE, &rendez);
                        watched.push((ch, s.tag == w::EVENTTYPE_FD_WRITE));
                    }
                }
                let timeout_ms = match soonest {
                    None => -1,
                    Some(ns) => {
                        i32::try_from((ns.saturating_add(999_999)) / 1_000_000).unwrap_or(i32::MAX)
                    }
                };
                let hit = purg_core::block::park_timeout(
                    &rendez,
                    || items.iter().any(|s| sub_ready(&c.prog, s)),
                    timeout_ms,
                );
                for (ch, _) in &watched {
                    ch.dev.unwatch(ch, &rendez);
                }
                hit?;
            } else if !ready_now && let Some(ns) = soonest.filter(|ns| *ns > 0) {
                // Through `park_timeout`, so that a kill ends the sleep.
                let ms = i32::try_from(ns.saturating_add(999_999) / 1_000_000).unwrap_or(i32::MAX);
                let slept = purg_core::block::park_timeout(
                    &purg_platform::sync::Rendez::new(),
                    || false,
                    ms,
                );
                killed_asleep = slept.is_err();
                slept?;
            }
            let fd_hit = items.iter().any(|s| sub_ready(&c.prog, s));
            let mut out = 0i32;
            for s in &items {
                let fire = if s.tag == w::EVENTTYPE_CLOCK {
                    if fd_hit {
                        s.rel == 0
                    } else {
                        Some(s.rel) == soonest
                    }
                } else {
                    sub_ready(&c.prog, s)
                };
                if !fire {
                    continue;
                }
                let at = events.wrapping_add(out * EVENT);
                mem.write(at, &[0; EVENT as usize])?;
                mem.write(at, &s.userdata.to_le_bytes())?;
                if s.tag != w::EVENTTYPE_CLOCK && c.prog.chan(s.fd).is_err() {
                    mem.write(at.wrapping_add(8), &(errno::BADF as u16).to_le_bytes())?;
                }
                mem.put_u8(at.wrapping_add(10), s.tag)?;
                out += 1;
            }
            mem.put_u32(nevents, out as u32)?;
            Ok(())
        });
        if killed_asleep {
            return Err(Stop::Exit(String::from("killed")));
        }
        errno
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

    pub fn clock_res_get(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        id: i32,
        resolution: i32,
    ) -> HostResult<i32> {
        call(ctx, |_| {
            match id as u32 {
                w::CLOCKID_REALTIME | w::CLOCKID_MONOTONIC => {}
                _ => return Err(Error::new(error::ENOTIMPL)),
            }
            mem.put_u64(resolution, 1)?;
            Ok(())
        })
    }

    /// Delivers a signal by killing the prog's note. The guest keeps running
    /// until its next blocking call, which then returns `EINTR`.
    pub fn proc_raise(ctx: &mut Ctx, _mem: &mut Mem<'_>, sig: i32) -> HostResult<i32> {
        call(ctx, |c| {
            if !(1..=15).contains(&sig) {
                return Err(Error::new(error::EBADARG));
            }
            c.prog.proc.kill();
            Ok(())
        })
    }

    fn net_sibling(path: &str, file: &str, sibling: &str) -> Result<String, Error> {
        let Some(dir) = path.strip_suffix(file) else {
            return Err(Error::new(error::ENOTSOCK));
        };
        Ok(alloc::format!("{dir}/{sibling}"))
    }

    /// `sock_accept` on an Inferno `listen` file: one read yields the new
    /// conversation, and the returned fd is its `data` file.
    pub fn sock_accept(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        _flags: i32,
        result: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let path = c.prog.fd2path(fd)?;
            if !path.ends_with("/listen") {
                return Err(Error::new(error::ENOTSOCK));
            }
            let mut buf = [0u8; 32];
            let n = c.prog.read(fd, &mut buf)?;
            let id = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::new(error::EIO));
            }
            let parent = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let proto = parent.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let data = alloc::format!("{proto}/{id}/data");
            let newfd = c.prog.open(&data, mode::ORDWR)?;
            mem.put_u32(result, newfd as u32)?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sock_recv(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        iovs: i32,
        n: i32,
        _flags: i32,
        nread: i32,
        roflags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let path = c.prog.fd2path(fd)?;
            if !path.ends_with("/data") {
                return Err(Error::new(error::ENOTSOCK));
            }
            let mut total = 0usize;
            if let Some(&(buf, len)) = iovecs(mem, iovs, n)?.iter().find(|v| v.1 != 0) {
                total = c.prog.read(fd, mem.slice_mut(buf, len)?)?;
            }
            mem.put_u32(nread, total as u32)?;
            mem.write(roflags, &0u16.to_le_bytes())?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sock_send(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        iovs: i32,
        n: i32,
        _flags: i32,
        nwritten: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let path = c.prog.fd2path(fd)?;
            if !path.ends_with("/data") {
                return Err(Error::new(error::ENOTSOCK));
            }
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

    pub fn sock_shutdown(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32, how: i32) -> HostResult<i32> {
        call(ctx, |c| {
            if how & !0x3 != 0 || how == 0 {
                return Err(Error::new(error::EBADARG));
            }
            let path = c.prog.fd2path(fd)?;
            let ctl = net_sibling(&path, "/data", "ctl")?;
            let w = c.prog.open(&ctl, mode::OWRITE)?;
            c.prog.write(w, b"hangup")?;
            c.prog.close(w)
        })
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

    pub fn iounit(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.iounit_of(fd).map(|n| n as i32))
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

    #[allow(clippy::too_many_arguments)]
    pub fn mount(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        afd: i32,
        old: i32,
        olen: i32,
        flags: i32,
        aname: i32,
        alen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog
                .mount(
                    fd,
                    afd,
                    mem.str(old, olen)?,
                    flags as u32,
                    mem.str(aname, alen)?,
                )
                .map(|()| 0)
        })
    }

    pub fn fauth(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        aname: i32,
        alen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| c.prog.fauth(fd, mem.str(aname, alen)?))
    }

    /// Returns the byte count even when the copy stopped on an error or a
    /// kill; the errstr then says why, as Inferno's `kread` would set it.
    pub fn stream(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        src: i32,
        dst: i32,
        bufsiz: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let (n, failed) = c.prog.stream(src, dst, bufsiz)?;
            if let Some(e) = failed {
                c.prog.set_error(&e);
            }
            Ok(i32::try_from(n).unwrap_or(i32::MAX))
        })
    }

    pub fn export(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        dir: i32,
        dlen: i32,
        flags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog
                .export(fd, mem.str(dir, dlen)?, flags as u32)
                .map(|()| 0)
        })
    }

    pub fn fversion(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        msize: i32,
        ver: i32,
        vlen: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let version = c.prog.fversion(fd, msize as u32, mem.str(ver, vlen)?)?;
            put_str(mem, ver, vlen, &version)
        })
    }

    pub fn file2chan(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dir: i32,
        dlen: i32,
        name: i32,
        nlen: i32,
        handles: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            mem.slice(handles, 8)?;
            let (r, w) = c
                .prog
                .file2chan(mem.str(dir, dlen)?, mem.str(name, nlen)?)?;
            mem.put_u32(handles, r as u32)?;
            mem.put_u32(handles.wrapping_add(4), w as u32)?;
            Ok(0)
        })
    }

    pub fn chan_new(ctx: &mut Ctx, _mem: &mut Mem<'_>, cap: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.chan_new(cap as u32))
    }

    pub fn chan_close(ctx: &mut Ctx, _mem: &mut Mem<'_>, h: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.chan_close(h).map(|()| 0))
    }

    pub fn chan_send(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        h: i32,
        buf: i32,
        n: i32,
        handles: i32,
        nh: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let bytes = mem.slice(buf, n)?.to_vec();
            let caps = get_i32s(mem, handles, nh)?;
            c.prog.chan_send(h, &bytes, &caps).map(|()| 0)
        })
    }

    pub fn chan_recv(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        h: i32,
        buf: i32,
        blen: i32,
        handles: i32,
        maxh: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let max = usize::try_from(blen).map_err(|_| Error::new(error::EBADARG))?;
            let Some((bytes, caps)) = c.prog.chan_recv(h, max)? else {
                return Err(Error::new(error::EHUNGUP));
            };
            mem.write(buf, &bytes)?;
            let slots = usize::try_from(maxh.max(0)).unwrap_or(0);
            for i in 0..slots {
                let id = caps.get(i).copied().unwrap_or(-1);
                mem.put_u32(handles.wrapping_add(4 * i as i32), id as u32)?;
            }
            Ok(bytes.len() as i32)
        })
    }

    pub fn alt(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        ops: i32,
        nops: i32,
        block: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            if nops < 0 {
                return Err(Error::new(error::EBADARG));
            }
            let mut arms = Vec::new();
            for i in 0..nops {
                let at = ops.wrapping_add(24 * i);
                let handle = mem.u32(at)? as i32;
                let kind = mem.u32(at.wrapping_add(4))? as i32;
                let buf = mem.u32(at.wrapping_add(8))? as i32;
                let len = mem.u32(at.wrapping_add(12))? as i32;
                let caps = mem.u32(at.wrapping_add(16))? as i32;
                let ncaps = mem.u32(at.wrapping_add(20))? as i32;
                let bytes = if kind == 0 {
                    Vec::new()
                } else {
                    mem.slice(buf, len)?.to_vec()
                };
                let cap_ids = if kind == 0 {
                    Vec::new()
                } else {
                    get_i32s(mem, caps, ncaps)?
                };
                arms.push((handle, kind != 0, bytes, cap_ids, buf, len, caps));
            }
            let plain: Vec<_> = arms
                .iter()
                .map(|a| (a.0, a.1, a.2.clone(), a.3.clone()))
                .collect();
            match c.prog.chan_alt(&plain, block != 0)? {
                None => Ok(-1),
                Some((i, bytes, caps)) => {
                    let arm = &arms[i];
                    if !arm.1 {
                        let n = bytes.len().min(arm.5.max(0) as usize);
                        mem.write(arm.4, &bytes[..n])?;
                        let at = ops.wrapping_add(24 * i as i32);
                        mem.put_u32(at.wrapping_add(12), n as u32)?;
                        for (k, id) in caps.iter().enumerate() {
                            mem.put_u32(arm.6.wrapping_add(4 * k as i32), *id as u32)?;
                        }
                    }
                    Ok(i as i32)
                }
            }
        })
    }

    pub fn wait(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        ops: i32,
        nops: i32,
        timeout_ms: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            if nops < 0 {
                return Err(Error::new(error::EBADARG));
            }
            let mut list = Vec::new();
            for i in 0..nops {
                let at = ops.wrapping_add(8 * i);
                list.push((mem.u32(at)? as i32, mem.u32(at.wrapping_add(4))? as i32));
            }
            Ok(match c.prog.wait_ops(&list, timeout_ms)? {
                Some(i) => i as i32,
                None => -1,
            })
        })
    }
}
