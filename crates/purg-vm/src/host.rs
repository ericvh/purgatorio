//! Host functions. Each takes the prog context, the guest memory and the raw
//! WASM arguments. Errors are recorded as the prog's errstr; WASI functions
//! then return the mapped errno and `purgatorio` functions return `-1`.

use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::error;
use purg_core::Error;
use purg_styx::Dir;

use crate::mem::{Mem, Ptr};
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
    ///
    /// A relative name that starts with `#` is a device name, as it is for
    /// `namec`. wasi-libc makes a relative name absolute with its copy of
    /// the current directory (kept equal to dot by the SDK) and passes it
    /// relative to the `/` preopen, so `#c/sysname` arrives as
    /// `usr/me/#c/sysname`: the part after dot is taken back out. An
    /// absolute name with a `#` element right after dot, or right after
    /// `/`, is read the same way, as such a file cannot be told apart.
    fn at_path(ctx: &Ctx, dirfd: i32, path: &str) -> Result<String, Error> {
        if path.starts_with('/') {
            return Ok(String::from(path));
        }
        if path.starts_with('#') {
            return Ok(String::from(path));
        }
        if dirfd == PREOPEN_ROOT_FD {
            let dot = ctx.prog.dot();
            let cwd = dot.trim_start_matches('/');
            if let Some(rest) = path.strip_prefix(cwd).and_then(|r| r.strip_prefix('/'))
                && !cwd.is_empty()
                && rest.starts_with('#')
            {
                return Ok(String::from(rest));
            }
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
        call(ctx, |c| {
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
                // A sleep. It must end early when the prog is killed, which
                // then ends the prog (see `host_call`).
                let rendez = purg_platform::sync::Rendez::new();
                let mut left = ns;
                while left > 0 {
                    let ms = i32::try_from(left.div_ceil(1_000_000)).unwrap_or(i32::MAX);
                    purg_core::block::park_timeout(&rendez, || false, ms)?;
                    left = left.saturating_sub(u64::from(ms.unsigned_abs()) * 1_000_000);
                }
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

    /// Delivers a signal by killing the prog's note: the prog ends, with
    /// status `killed`, as this call returns.
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
    pub fn errstr<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, buf: P, len: P) -> HostResult<i32> {
        call(ctx, |c| {
            let s = c.prog.errstr.as_bytes();
            let n = s.len().min(len.size());
            mem.write(buf, &s[..n])?;
            Ok(n as i32)
        })
    }

    pub fn werrstr<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, ptr: P, len: P) -> HostResult<i32> {
        call(ctx, |c| {
            let s = String::from(mem.str(ptr, len)?);
            c.prog.errstr = s;
            Ok(0)
        })
    }

    pub fn exits<P: Ptr>(_ctx: &mut Ctx, mem: &mut Mem<'_>, ptr: P, len: P) -> HostResult<()> {
        let status = match mem.str(ptr, len) {
            Ok(s) => String::from(s),
            Err(e) => String::from(e.as_str()),
        };
        Err(Stop::Exit(status))
    }

    fn put_dir<P: Ptr>(mem: &mut Mem<'_>, buf: P, len: P, d: &Dir) -> Result<i32, Error> {
        let packed = d.pack().map_err(|_| Error::new(error::EIO))?;
        if packed.len() > len.size() {
            return Err(Error::new(error::ESHORTSTAT));
        }
        mem.write(buf, &packed)?;
        Ok(packed.len() as i32)
    }

    pub fn stat<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: P,
        plen: P,
        buf: P,
        len: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = c.prog.stat(mem.str(path, plen)?)?;
            put_dir(mem, buf, len, &d)
        })
    }

    pub fn fstat<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = c.prog.fstat(fd)?;
            put_dir(mem, buf, len, &d)
        })
    }

    pub fn dirread<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let out = mem.slice_mut(buf, len)?;
            Ok(c.prog.dirread(fd, out)? as i32)
        })
    }

    fn put_str<P: Ptr>(mem: &mut Mem<'_>, buf: P, len: P, v: &str) -> Result<i32, Error> {
        if v.len() > len.size() {
            return Err(Error::new(error::ESHORTSTAT));
        }
        mem.write(buf, v.as_bytes())?;
        Ok(v.len() as i32)
    }

    fn get_dir<P: Ptr>(mem: &Mem<'_>, ptr: P, len: P) -> Result<Dir, Error> {
        Dir::unpack(mem.slice(ptr, len)?)
            .map(|(d, _)| d)
            .map_err(|_| Error::new(error::EBADARG))
    }

    fn get_i32s<P: Ptr>(mem: &Mem<'_>, ptr: P, n: P) -> Result<Vec<i32>, Error> {
        (0..count(n)?)
            .map(|i| Ok(mem.u32(ptr.off(4 * i))? as i32))
            .collect()
    }

    /// A count of elements, which in either width is at most `i32::MAX`:
    /// a negative wasm32 count is refused here too.
    fn count<P: Ptr>(n: P) -> Result<u64, Error> {
        match n.addr() {
            n if n <= i32::MAX as u64 => Ok(n),
            _ => Err(Error::new(error::EBADARG)),
        }
    }

    fn omode(m: i32) -> Result<u8, Error> {
        u8::try_from(m).map_err(|_| Error::new(error::EBADARG))
    }

    pub fn open<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: P,
        plen: P,
        m: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| c.prog.open(mem.str(path, plen)?, omode(m)?))
    }

    pub fn create<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: P,
        plen: P,
        m: i32,
        perm: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog.create(mem.str(path, plen)?, omode(m)?, perm as u32)
        })
    }

    pub fn wstat<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: P,
        plen: P,
        dir: P,
        dlen: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = get_dir(mem, dir, dlen)?;
            c.prog.wstat(mem.str(path, plen)?, &d).map(|()| 0)
        })
    }

    pub fn fwstat<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        dir: P,
        dlen: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let d = get_dir(mem, dir, dlen)?;
            c.prog.fwstat(fd, &d).map(|()| 0)
        })
    }

    pub fn remove<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, path: P, plen: P) -> HostResult<i32> {
        call(ctx, |c| c.prog.remove(mem.str(path, plen)?).map(|()| 0))
    }

    pub fn fd2path<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
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

    pub fn chdir<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, path: P, plen: P) -> HostResult<i32> {
        call(ctx, |c| c.prog.chdir(mem.str(path, plen)?).map(|()| 0))
    }

    pub fn dup(ctx: &mut Ctx, _mem: &mut Mem<'_>, old: i32, new: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.dup(old, new))
    }

    pub fn pipe<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, fds: P) -> HostResult<i32> {
        call(ctx, |c| {
            mem.slice(fds, P::at(8))?;
            let [a, b] = c.prog.pipe()?;
            mem.put_u32(fds, a as u32)?;
            mem.put_u32(fds.off(4), b as u32)?;
            Ok(0)
        })
    }

    pub fn iounit(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.iounit_of(fd).map(|n| n as i32))
    }

    /// A mount id as `bind` and `mount` return it: positive, as an
    /// Inferno `int`.
    fn mount_id(id: u64) -> i32 {
        i32::try_from(id).unwrap_or(i32::MAX)
    }

    pub fn bind<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        name: P,
        nlen: P,
        old: P,
        olen: P,
        flags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog
                .bind(mem.str(name, nlen)?, mem.str(old, olen)?, flags as u32)
                .map(mount_id)
        })
    }

    pub fn unmount<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        name: P,
        nlen: P,
        old: P,
        olen: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let name = if nlen.addr() == 0 {
                None
            } else {
                Some(mem.str(name, nlen)?)
            };
            c.prog.unmount(name, mem.str(old, olen)?).map(|()| 0)
        })
    }

    pub fn pctl<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        flags: i32,
        movefd: P,
        n: P,
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
    pub fn spawn_prog<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: P,
        plen: P,
        argv: P,
        alen: P,
        flags: i32,
        fdmap: P,
        n: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let path = crate::command_path(mem.str(path, plen)?);
            let args: Vec<String> = mem
                .str(argv, alen)?
                .split_terminator('\0')
                .map(String::from)
                .collect();
            let pairs = get_i32s(mem, fdmap, P::at(count(n)? * 2))?;
            let map: Vec<(i32, i32)> = pairs.chunks(2).map(|p| (p[0], p[1])).collect();
            let engine = c.engine.clone();
            let pid = crate::spawn(&c.prog, &engine, &path, args, flags as u32, &map)?;
            Ok(pid as i32)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mount<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        afd: i32,
        old: P,
        olen: P,
        flags: i32,
        aname: P,
        alen: P,
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
                .map(mount_id)
        })
    }

    pub fn fauth<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        aname: P,
        alen: P,
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

    pub fn export<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        dir: P,
        dlen: P,
        flags: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog
                .export(fd, mem.str(dir, dlen)?, flags as u32)
                .map(|()| 0)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn export_key<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        dir: P,
        dlen: P,
        flags: i32,
        key: P,
        klen: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let dir = mem.str(dir, dlen)?;
            let key = mem.str(key, klen)?;
            c.prog.export_key(fd, dir, flags as u32, key).map(|()| 0)
        })
    }

    pub fn fversion<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        msize: i32,
        ver: P,
        vlen: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let version = c.prog.fversion(fd, msize as u32, mem.str(ver, vlen)?)?;
            put_str(mem, ver, vlen, &version)
        })
    }

    pub fn file2chan<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        dir: P,
        dlen: P,
        name: P,
        nlen: P,
        handles: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            mem.slice(handles, P::at(8))?;
            let (r, w) = c
                .prog
                .file2chan(mem.str(dir, dlen)?, mem.str(name, nlen)?)?;
            mem.put_u32(handles, r as u32)?;
            mem.put_u32(handles.off(4), w as u32)?;
            Ok(0)
        })
    }

    pub fn chan_new(ctx: &mut Ctx, _mem: &mut Mem<'_>, cap: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.chan_new(cap as u32))
    }

    pub fn chan_close(ctx: &mut Ctx, _mem: &mut Mem<'_>, h: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.chan_close(h).map(|()| 0))
    }

    pub fn chan_send<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        h: i32,
        buf: P,
        n: P,
        handles: P,
        nh: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let bytes = mem.slice(buf, n)?.to_vec();
            let caps = get_i32s(mem, handles, nh)?;
            c.prog.chan_send(h, &bytes, &caps).map(|()| 0)
        })
    }

    pub fn chan_recv<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        h: i32,
        buf: P,
        blen: P,
        handles: P,
        maxh: P,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let max = count(blen)? as usize;
            let Some((bytes, caps)) = c.prog.chan_recv(h, max)? else {
                return Err(Error::new(error::EHUNGUP));
            };
            mem.write(buf, &bytes)?;
            let slots = count(maxh).unwrap_or(0) as usize;
            for i in 0..slots {
                let id = caps.get(i).copied().unwrap_or(-1);
                mem.put_u32(handles.off(4 * i as u64), id as u32)?;
            }
            Ok(bytes.len() as i32)
        })
    }

    /// One arm of an `alt` as the guest lays it out: `handle[4] kind[4]`,
    /// then `buf len caps ncaps`, each 4 bytes in a wasm32 guest (24 bytes
    /// an arm) and 8 in a wasm64 one (40 bytes).
    struct Arm<P> {
        handle: i32,
        send: bool,
        buf: P,
        len: P,
        caps: P,
        ncaps: P,
    }

    impl<P: Ptr> Arm<P> {
        const WORD: u64 = if P::WIDE { 8 } else { 4 };
        const SIZE: u64 = 8 + 4 * Self::WORD;

        fn word(mem: &Mem<'_>, at: P) -> Result<P, Error> {
            Ok(P::at(if P::WIDE {
                mem.u64(at)?
            } else {
                u64::from(mem.u32(at)?)
            }))
        }

        fn read(mem: &Mem<'_>, at: P) -> Result<Arm<P>, Error> {
            Ok(Arm {
                handle: mem.u32(at)? as i32,
                send: mem.u32(at.off(4))? != 0,
                buf: Self::word(mem, at.off(8))?,
                len: Self::word(mem, at.off(8 + Self::WORD))?,
                caps: Self::word(mem, at.off(8 + 2 * Self::WORD))?,
                ncaps: Self::word(mem, at.off(8 + 3 * Self::WORD))?,
            })
        }

        /// Stores the length received in the arm at `at`.
        fn put_len(mem: &mut Mem<'_>, at: P, n: usize) -> Result<(), Error> {
            let at = at.off(8 + Self::WORD);
            if P::WIDE {
                mem.put_u64(at, n as u64)?;
            } else {
                mem.put_u32(at, n as u32)?;
            }
            Ok(())
        }
    }

    pub fn alt<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        ops: P,
        nops: P,
        block: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut arms = Vec::new();
            let mut plain = Vec::new();
            for i in 0..count(nops)? {
                let arm = Arm::read(mem, ops.off(Arm::<P>::SIZE * i))?;
                let (bytes, cap_ids) = if arm.send {
                    (
                        mem.slice(arm.buf, arm.len)?.to_vec(),
                        get_i32s(mem, arm.caps, arm.ncaps)?,
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
                plain.push((arm.handle, arm.send, bytes, cap_ids));
                arms.push(arm);
            }
            match c.prog.chan_alt(&plain, block != 0)? {
                None => Ok(-1),
                Some((i, bytes, caps)) => {
                    let arm = &arms[i];
                    if !arm.send {
                        // A negative wasm32 length holds nothing.
                        let room = count(arm.len).unwrap_or(0) as usize;
                        let n = bytes.len().min(room);
                        mem.write(arm.buf, &bytes[..n])?;
                        Arm::put_len(mem, ops.off(Arm::<P>::SIZE * i as u64), n)?;
                        for (k, id) in caps.iter().enumerate() {
                            mem.put_u32(arm.caps.off(4 * k as u64), *id as u32)?;
                        }
                    }
                    Ok(i as i32)
                }
            }
        })
    }

    pub fn wait<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        ops: P,
        nops: P,
        timeout_ms: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let mut list = Vec::new();
            for i in 0..count(nops)? {
                let at = ops.off(8 * i);
                list.push((mem.u32(at)? as i32, mem.u32(at.off(4))? as i32));
            }
            Ok(match c.prog.wait_ops(&list, timeout_ms)? {
                Some(i) => i as i32,
                None => -1,
            })
        })
    }

    // The calls below stand in for WASI in a wasm64 guest, which has no
    // `wasi_snapshot_preview1` (`docs/wasm64.md`). They are registered
    // only in the `purgatorio64` module.

    /// [`call`] for a call that returns an `i64`.
    fn call64(ctx: &mut Ctx, f: impl FnOnce(&mut Ctx) -> Result<i64, Error>) -> HostResult<i64> {
        Ok(match f(ctx) {
            Ok(n) => n,
            Err(e) => {
                ctx.prog.set_error(&e);
                -1
            }
        })
    }

    /// The most bytes one read or write moves, so that the count fits the
    /// `i32` it is returned in. A guest asks again for the rest.
    const MAX_IO: usize = i32::MAX as usize;

    /// The buffer of a read: all of it must be the guest's, though at most
    /// [`MAX_IO`] bytes of it are filled.
    fn io_mut<'m, P: Ptr>(mem: &'m mut Mem<'_>, buf: P, len: P) -> Result<&'m mut [u8], Error> {
        let b = mem.slice_mut(buf, len)?;
        let n = b.len().min(MAX_IO);
        Ok(&mut b[..n])
    }

    fn io<'m, P: Ptr>(mem: &'m Mem<'_>, buf: P, len: P) -> Result<&'m [u8], Error> {
        let b = mem.slice(buf, len)?;
        Ok(&b[..b.len().min(MAX_IO)])
    }

    /// `sys->read`: one read, which may be short.
    pub fn read<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
    ) -> HostResult<i32> {
        call(ctx, |c| Ok(c.prog.read(fd, io_mut(mem, buf, len)?)? as i32))
    }

    pub fn write<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
    ) -> HostResult<i32> {
        call(ctx, |c| Ok(c.prog.write(fd, io(mem, buf, len)?)? as i32))
    }

    fn offset(off: i64) -> Result<u64, Error> {
        u64::try_from(off).map_err(|_| Error::new(error::EBADARG))
    }

    pub fn pread<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
        off: i64,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let off = offset(off)?;
            Ok(c.prog.pread(fd, io_mut(mem, buf, len)?, off)? as i32)
        })
    }

    pub fn pwrite<P: Ptr>(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        fd: i32,
        buf: P,
        len: P,
        off: i64,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let off = offset(off)?;
            Ok(c.prog.pwrite(fd, io(mem, buf, len)?, off)? as i32)
        })
    }

    /// `sys->seek`: the new offset, or -1.
    pub fn seek(
        ctx: &mut Ctx,
        _mem: &mut Mem<'_>,
        fd: i32,
        off: i64,
        whence: i32,
    ) -> HostResult<i64> {
        call64(ctx, |c| {
            let whence = u8::try_from(whence).map_err(|_| Error::new(error::EBADARG))?;
            let at = c.prog.seek(fd, off, whence)?;
            i64::try_from(at).map_err(|_| Error::new(error::EBADARG))
        })
    }

    pub fn close(ctx: &mut Ctx, _mem: &mut Mem<'_>, fd: i32) -> HostResult<i32> {
        call(ctx, |c| c.prog.close(fd).map(|()| 0))
    }

    /// The prog's arguments, each followed by a NUL, as `spawn_prog` takes
    /// them. Returns their size, and copies them if `len` holds them: a
    /// guest calls with no buffer for the size, then again.
    pub fn args<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, buf: P, len: P) -> HostResult<i32> {
        call(ctx, |c| {
            let mut packed = Vec::new();
            for a in &c.argv {
                packed.extend_from_slice(a.as_bytes());
                packed.push(0);
            }
            let size = i32::try_from(packed.len()).map_err(|_| Error::new(error::EBADARG))?;
            if packed.len() <= len.size() {
                mem.write(buf, &packed)?;
            }
            Ok(size)
        })
    }

    /// `sys->sleep`. A kill ends the sleep, and the prog with it (see
    /// `host_call`).
    pub fn sleep(ctx: &mut Ctx, _mem: &mut Mem<'_>, ms: i32) -> HostResult<i32> {
        call(ctx, |_| {
            if ms > 0 {
                let rendez = purg_platform::sync::Rendez::new();
                purg_core::block::park_timeout(&rendez, || false, ms)?;
            }
            Ok(0)
        })
    }

    /// Nanoseconds since the epoch.
    pub fn nsec(ctx: &mut Ctx, _mem: &mut Mem<'_>) -> HostResult<i64> {
        Ok(ctx.prog.platform().now_ns() as i64)
    }

    /// Nanoseconds on the monotonic clock.
    pub fn mono(ctx: &mut Ctx, _mem: &mut Mem<'_>) -> HostResult<i64> {
        Ok(ctx.prog.platform().monotonic_ns() as i64)
    }

    /// Fills the buffer with random bytes.
    pub fn random<P: Ptr>(ctx: &mut Ctx, mem: &mut Mem<'_>, buf: P, len: P) -> HostResult<i32> {
        call(ctx, |c| {
            c.prog.platform().entropy(mem.slice_mut(buf, len)?);
            Ok(0)
        })
    }
}
