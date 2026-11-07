//! The runtime-agnostic half of running a WASM prog.
//!
//! Host functions are written once here, against [`Mem`] (bounds-checked
//! guest memory) and [`Ctx`] (the prog). Each backend (`purg-vm-wasmtime`,
//! `purg-vm-wasmi`) registers them with [`for_each_import!`] and implements
//! [`Engine`]. The WASI preview1 subset is implemented natively over the
//! Purgatorio namespace, so it is identical on the emu and on bare metal.
#![no_std]

extern crate alloc;

mod host;
mod loader;
mod mem;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::mode;
use purg_core::{Chan, Error, Prog};

pub use host::{purgatorio, wasi};
pub use loader::{Malformed, abi_version, load_module};
pub use mem::{Fault, Mem};

/// Why the guest stopped before returning from its entry point. Host
/// functions return this as an error to unwind the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `purgatorio.exits` or `proc_exit`, with the Inferno status string.
    Exit(String),
}

impl core::fmt::Display for Stop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Stop::Exit(s) => write!(f, "exit {s:?}"),
        }
    }
}

impl core::error::Error for Stop {}

pub type HostResult<T> = Result<T, Stop>;

/// Host-side state of a running prog: Inferno's `Prog` plus the WASI view of
/// its arguments and environment, and the engine that runs it (and any
/// progs it spawns).
pub struct Ctx {
    pub prog: Prog,
    pub argv: Vec<String>,
    /// `name=value` strings, a snapshot of the prog's Egrp at start.
    pub env: Vec<String>,
    pub engine: Arc<dyn Engine>,
}

impl Ctx {
    pub fn new(prog: Prog, argv: Vec<String>, engine: Arc<dyn Engine>) -> Ctx {
        let env = prog.egrp().environ();
        Ctx {
            prog,
            argv,
            env,
            engine,
        }
    }
}

/// A WASM runtime backend.
pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;
    /// Instantiates `wasm` against the imports in [`for_each_import!`] and
    /// calls its entry point, returning how it stopped. A normal return from
    /// the entry point is `Stop::Exit("")`; a trap is `Stop::Exit` with a
    /// description.
    /// The context is moved into the runtime's store and handed back.
    fn run(&self, wasm: &[u8], ctx: Ctx) -> (Ctx, Result<Stop, Error>);
}

/// Opens the standard fds every command starts with: 0, 1 and 2 on
/// `/dev/cons`, then the WASI preopens `.` (fd 3) and `/` (fd 4).
pub fn init_fds(prog: &mut Prog) -> Result<(), Error> {
    for (want, path, omode) in [
        (0, "/dev/cons", mode::OREAD),
        (1, "/dev/cons", mode::OWRITE),
        (2, "/dev/cons", mode::OWRITE),
    ] {
        let fd = prog.open(path, omode)?;
        debug_assert_eq!(fd, want);
    }
    init_preopens(prog)
}

/// Installs the WASI preopens: `.` at fd 3 and `/` at fd 4. wasi-libc stops
/// scanning for preopens at the first fd that is not one, so these two
/// numbers are reserved in every prog.
pub fn init_preopens(prog: &mut Prog) -> Result<(), Error> {
    for (fd, path) in [
        (purg_abi::PREOPEN_ROOT_FD, "/"),
        (purg_abi::PREOPEN_DOT_FD, "."),
    ] {
        let c: Chan = prog.open_chan(path, mode::OREAD)?;
        prog.fgrp.install(fd, Arc::new(c))?;
    }
    Ok(())
}

/// Maps a command name to its binary: `cat` is `/bin/cat.wasm`; names
/// containing `/` are used as given.
pub fn command_path(cmd: &str) -> String {
    if cmd.contains('/') {
        String::from(cmd)
    } else {
        alloc::format!("/bin/{cmd}.wasm")
    }
}

/// The prog name shown in exit messages: the module's base name without
/// `.wasm`.
pub fn module_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".wasm").unwrap_or(base)
}

/// Runs an already loaded module in `ctx`'s prog until it exits. Returns the
/// Inferno exit status (`""` is success), or an error prefixed with `path`.
pub fn run_module(ctx: Ctx, wasm: &[u8], path: &str) -> (Ctx, Result<String, Error>) {
    let platform = ctx.prog.kernel.platform.clone();
    platform.set_current_pid(ctx.prog.pid);
    ctx.prog.proc.set_name(module_name(path));
    let engine = ctx.engine.clone();
    let (ctx, res) = engine.run(wasm, ctx);
    let res = match res {
        Ok(Stop::Exit(status)) => Ok(status),
        Err(e) => Err(Error::from(alloc::format!("{path}: {e}"))),
    };
    (ctx, res)
}

/// Loads the command at `path` through the prog's namespace and runs it.
pub fn run_command(mut ctx: Ctx, path: &str) -> (Ctx, Result<String, Error>) {
    match load_module(&mut ctx.prog, path) {
        Ok(wasm) => run_module(ctx, &wasm, path),
        Err(e) => (ctx, Err(Error::from(alloc::format!("{path}: {e}")))),
    }
}

/// `spawn_prog`: starts `path` in a new prog on a new kproc. The child
/// shares `parent`'s groups except as `flags` say (see [`Prog::spawn`]),
/// gets `fdmap`'s fds, and has the WASI preopens reinstalled. The module is
/// loaded before the kproc starts, so a missing or bad module is the
/// caller's error. Returns the child's pid.
pub fn spawn(
    parent: &Prog,
    engine: &Arc<dyn Engine>,
    path: &str,
    argv: Vec<String>,
    flags: u32,
    fdmap: &[(i32, i32)],
) -> Result<u32, Error> {
    if fdmap
        .iter()
        .any(|&(_, to)| to == purg_abi::PREOPEN_ROOT_FD || to == purg_abi::PREOPEN_DOT_FD)
    {
        return Err(Error::new(purg_abi::error::EBADARG));
    }
    let mut child = parent.spawn(flags, fdmap)?;
    let wasm =
        load_module(&mut child, path).map_err(|e| Error::from(alloc::format!("{path}: {e}")))?;
    init_preopens(&mut child)?;
    let pid = child.pid;
    let platform = child.kernel.platform.clone();
    let ctx = Ctx::new(child, argv, engine.clone());
    let path = String::from(path);
    let name = String::from(module_name(&path));
    platform.spawn_kproc(
        &name,
        Box::new(move || {
            let mut ctx = ctx;
            ctx.prog.started();
            let (mut ctx, res) = run_module(ctx, &wasm, &path);
            ctx.prog.status = match res {
                Ok(s) => s,
                Err(e) => {
                    let msg = alloc::format!("{e}\n");
                    let _ = ctx.prog.write(2, msg.as_bytes());
                    alloc::format!("fail:{e}")
                }
            };
        }),
    )?;
    Ok(pid)
}

/// Invokes `$m!` once per import, as
/// `$m!(module, "name", path::to::host_fn, (arg: type, ...) -> ret)`.
/// Backends use it to register every host function with their linker.
#[macro_export]
macro_rules! for_each_import {
    ($m:ident) => {
        $m!($crate::WASI, "args_sizes_get", $crate::wasi::args_sizes_get, (argc: i32, size: i32) -> i32);
        $m!($crate::WASI, "args_get", $crate::wasi::args_get, (argv: i32, buf: i32) -> i32);
        $m!($crate::WASI, "environ_sizes_get", $crate::wasi::environ_sizes_get, (count: i32, size: i32) -> i32);
        $m!($crate::WASI, "environ_get", $crate::wasi::environ_get, (env: i32, buf: i32) -> i32);
        $m!($crate::WASI, "fd_read", $crate::wasi::fd_read, (fd: i32, iovs: i32, n: i32, nread: i32) -> i32);
        $m!($crate::WASI, "fd_write", $crate::wasi::fd_write, (fd: i32, iovs: i32, n: i32, nwritten: i32) -> i32);
        $m!($crate::WASI, "fd_close", $crate::wasi::fd_close, (fd: i32) -> i32);
        $m!($crate::WASI, "fd_seek", $crate::wasi::fd_seek, (fd: i32, offset: i64, whence: i32, newoff: i32) -> i32);
        $m!($crate::WASI, "fd_fdstat_get", $crate::wasi::fd_fdstat_get, (fd: i32, buf: i32) -> i32);
        $m!($crate::WASI, "fd_prestat_get", $crate::wasi::fd_prestat_get, (fd: i32, buf: i32) -> i32);
        $m!($crate::WASI, "fd_prestat_dir_name", $crate::wasi::fd_prestat_dir_name, (fd: i32, path: i32, len: i32) -> i32);
        $m!($crate::WASI, "fd_filestat_get", $crate::wasi::fd_filestat_get, (fd: i32, buf: i32) -> i32);
        $m!($crate::WASI, "fd_readdir", $crate::wasi::fd_readdir, (fd: i32, buf: i32, len: i32, cookie: i64, used: i32) -> i32);
        $m!($crate::WASI, "path_open", $crate::wasi::path_open, (dirfd: i32, dirflags: i32, path: i32, plen: i32, oflags: i32, rights: i64, inheriting: i64, fdflags: i32, fd: i32) -> i32);
        $m!($crate::WASI, "path_filestat_get", $crate::wasi::path_filestat_get, (dirfd: i32, flags: i32, path: i32, plen: i32, buf: i32) -> i32);
        $m!($crate::WASI, "fd_pread", $crate::wasi::fd_pread, (fd: i32, iovs: i32, n: i32, offset: i64, nread: i32) -> i32);
        $m!($crate::WASI, "fd_pwrite", $crate::wasi::fd_pwrite, (fd: i32, iovs: i32, n: i32, offset: i64, nwritten: i32) -> i32);
        $m!($crate::WASI, "fd_tell", $crate::wasi::fd_tell, (fd: i32, offset: i32) -> i32);
        $m!($crate::WASI, "fd_fdstat_set_flags", $crate::wasi::fd_fdstat_set_flags, (fd: i32, flags: i32) -> i32);
        $m!($crate::WASI, "fd_filestat_set_size", $crate::wasi::fd_filestat_set_size, (fd: i32, size: i64) -> i32);
        $m!($crate::WASI, "fd_filestat_set_times", $crate::wasi::fd_filestat_set_times, (fd: i32, atim: i64, mtim: i64, fst: i32) -> i32);
        $m!($crate::WASI, "fd_sync", $crate::wasi::fd_sync, (fd: i32) -> i32);
        $m!($crate::WASI, "fd_datasync", $crate::wasi::fd_sync, (fd: i32) -> i32);
        $m!($crate::WASI, "fd_renumber", $crate::wasi::fd_renumber, (from: i32, to: i32) -> i32);
        $m!($crate::WASI, "path_create_directory", $crate::wasi::path_create_directory, (dirfd: i32, path: i32, plen: i32) -> i32);
        $m!($crate::WASI, "path_remove_directory", $crate::wasi::path_remove_directory, (dirfd: i32, path: i32, plen: i32) -> i32);
        $m!($crate::WASI, "path_unlink_file", $crate::wasi::path_unlink_file, (dirfd: i32, path: i32, plen: i32) -> i32);
        $m!($crate::WASI, "path_rename", $crate::wasi::path_rename, (oldfd: i32, old: i32, olen: i32, newfd: i32, new: i32, nlen: i32) -> i32);
        $m!($crate::WASI, "path_filestat_set_times", $crate::wasi::path_filestat_set_times, (dirfd: i32, flags: i32, path: i32, plen: i32, atim: i64, mtim: i64, fst: i32) -> i32);
        $m!($crate::WASI, "path_readlink", $crate::wasi::path_readlink, (dirfd: i32, path: i32, plen: i32, buf: i32, blen: i32, used: i32) -> i32);
        $m!($crate::WASI, "poll_oneoff", $crate::wasi::poll_oneoff, (subs: i32, events: i32, n: i32, nevents: i32) -> i32);
        $m!($crate::WASI, "sched_yield", $crate::wasi::sched_yield, () -> i32);
        $m!($crate::WASI, "clock_time_get", $crate::wasi::clock_time_get, (id: i32, precision: i64, time: i32) -> i32);
        $m!($crate::WASI, "random_get", $crate::wasi::random_get, (buf: i32, len: i32) -> i32);
        $m!($crate::WASI, "proc_exit", $crate::wasi::proc_exit, (code: i32) -> ());
        $m!($crate::PURGATORIO, "errstr", $crate::purgatorio::errstr, (buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "werrstr", $crate::purgatorio::werrstr, (ptr: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "exits", $crate::purgatorio::exits, (ptr: i32, len: i32) -> ());
        $m!($crate::PURGATORIO, "stat", $crate::purgatorio::stat, (path: i32, plen: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "fstat", $crate::purgatorio::fstat, (fd: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "dirread", $crate::purgatorio::dirread, (fd: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "open", $crate::purgatorio::open, (path: i32, plen: i32, mode: i32) -> i32);
        $m!($crate::PURGATORIO, "create", $crate::purgatorio::create, (path: i32, plen: i32, mode: i32, perm: i32) -> i32);
        $m!($crate::PURGATORIO, "wstat", $crate::purgatorio::wstat, (path: i32, plen: i32, dir: i32, dlen: i32) -> i32);
        $m!($crate::PURGATORIO, "fwstat", $crate::purgatorio::fwstat, (fd: i32, dir: i32, dlen: i32) -> i32);
        $m!($crate::PURGATORIO, "remove", $crate::purgatorio::remove, (path: i32, plen: i32) -> i32);
        $m!($crate::PURGATORIO, "fd2path", $crate::purgatorio::fd2path, (fd: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "chdir", $crate::purgatorio::chdir, (path: i32, plen: i32) -> i32);
        $m!($crate::PURGATORIO, "dup", $crate::purgatorio::dup, (old: i32, new: i32) -> i32);
        $m!($crate::PURGATORIO, "pipe", $crate::purgatorio::pipe, (fds: i32) -> i32);
        $m!($crate::PURGATORIO, "iounit", $crate::purgatorio::iounit, (fd: i32) -> i32);
        $m!($crate::PURGATORIO, "bind", $crate::purgatorio::bind, (name: i32, nlen: i32, old: i32, olen: i32, flags: i32) -> i32);
        $m!($crate::PURGATORIO, "unmount", $crate::purgatorio::unmount, (name: i32, nlen: i32, old: i32, olen: i32) -> i32);
        $m!($crate::PURGATORIO, "pctl", $crate::purgatorio::pctl, (flags: i32, movefd: i32, n: i32) -> i32);
        $m!($crate::PURGATORIO, "spawn_prog", $crate::purgatorio::spawn_prog, (path: i32, plen: i32, argv: i32, alen: i32, flags: i32, fdmap: i32, n: i32) -> i32);
    };
}

#[doc(hidden)]
pub const WASI: &str = purg_abi::wasi::MODULE;
#[doc(hidden)]
pub const PURGATORIO: &str = purg_abi::MODULE;
