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
            mem.put_u8(buf, filetype(ch))?;
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
            let ch = c.prog.chan(fd)?.clone();
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
            ctx.prog.dot.clone()
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
        _fdflags: i32,
        fd: i32,
    ) -> HostResult<i32> {
        call(ctx, |c| {
            let p = at_path(c, dirfd, mem.str(path, plen)?)?;
            let oflags = oflags as u16;
            if oflags & (w::OFLAGS_CREAT | w::OFLAGS_EXCL) != 0 {
                // M1 namespaces are read-only; create arrives with #U write
                // support.
                return Err(Error::new(error::ENOTIMPL));
            }
            let ch = c.prog.namec(&p)?;
            if oflags & w::OFLAGS_DIRECTORY != 0 && !ch.is_dir() {
                return Err(Error::new(error::ENOTDIR));
            }
            let rights = rights as u64;
            let write = rights & w::RIGHTS_FD_WRITE != 0;
            let read = rights & w::RIGHTS_FD_READ != 0;
            let mut omode = match (read, write) {
                _ if ch.is_dir() => mode::OREAD,
                (true, true) => mode::ORDWR,
                (false, true) => mode::OWRITE,
                _ => mode::OREAD,
            };
            if oflags & w::OFLAGS_TRUNC != 0 {
                omode |= mode::OTRUNC;
            }
            let newfd = c.prog.open(&p, omode)?;
            mem.put_u32(fd, newfd as u32)?;
            Ok(())
        })
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

    /// Replaces the running module. Returns `-1` only for bad arguments;
    /// load errors surface from the driver once the old module is gone.
    pub fn exec(
        ctx: &mut Ctx,
        mem: &mut Mem<'_>,
        path: i32,
        plen: i32,
        argv: i32,
        alen: i32,
    ) -> HostResult<i32> {
        let parsed = (|| -> Result<(String, Vec<String>), Error> {
            let p = String::from(mem.str(path, plen)?);
            let raw = mem.str(argv, alen)?;
            let args = raw.split_terminator('\0').map(String::from).collect();
            Ok((p, args))
        })();
        match parsed {
            Ok((path, argv)) => Err(Stop::Exec { path, argv }),
            Err(e) => {
                ctx.prog.set_error(&e);
                Ok(-1)
            }
        }
    }
}
