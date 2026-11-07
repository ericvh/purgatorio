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

use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::mode;
use purg_core::{Error, Prog};

pub use host::{purgatorio, wasi};
pub use loader::{Malformed, abi_version, load_module};
pub use mem::{Fault, Mem};

/// Why the guest stopped before returning from its entry point. Host
/// functions return this as an error to unwind the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `purgatorio.exits` or `proc_exit`, with the Inferno status string.
    Exit(String),
    /// `purgatorio.exec`: replace the module in the same prog.
    Exec { path: String, argv: Vec<String> },
}

impl core::fmt::Display for Stop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Stop::Exit(s) => write!(f, "exit {s:?}"),
            Stop::Exec { path, .. } => write!(f, "exec {path}"),
        }
    }
}

impl core::error::Error for Stop {}

pub type HostResult<T> = Result<T, Stop>;

/// Host-side state of a running prog: Inferno's `Prog` plus the WASI view of
/// its arguments and environment.
pub struct Ctx {
    pub prog: Prog,
    pub argv: Vec<String>,
    pub env: Vec<String>,
}

impl Ctx {
    pub fn new(prog: Prog, argv: Vec<String>) -> Ctx {
        Ctx {
            prog,
            argv,
            env: Vec::new(),
        }
    }
}

/// A WASM runtime backend.
pub trait Engine {
    fn name(&self) -> &'static str;
    /// Instantiates `wasm` against the imports in [`for_each_import!`] and
    /// calls its entry point, returning how it stopped. A normal return from
    /// the entry point is `Stop::Exit("")`; a trap is `Stop::Exit` with a
    /// description.
    /// The context is moved into the runtime's store and handed back.
    fn run(&self, wasm: &[u8], ctx: Ctx) -> (Ctx, Result<Stop, Error>);
}

/// Opens the standard fds every command starts with: 0, 1 and 2 on
/// `/dev/cons`, then the WASI preopens `/` (fd 3) and `.` (fd 4).
pub fn init_fds(prog: &mut Prog) -> Result<(), Error> {
    for (want, path, omode) in [
        (0, "/dev/cons", mode::OREAD),
        (1, "/dev/cons", mode::OWRITE),
        (2, "/dev/cons", mode::OWRITE),
        (purg_abi::PREOPEN_ROOT_FD, "/", mode::OREAD),
        (purg_abi::PREOPEN_DOT_FD, ".", mode::OREAD),
    ] {
        let fd = prog.open(path, omode)?;
        debug_assert_eq!(fd, want);
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

/// Runs the command at `path` in `ctx`'s prog until it exits, following
/// `exec` requests. Returns the Inferno exit status (`""` is success), or
/// an error prefixed with the path of the module that failed to load.
pub fn run_command(engine: &dyn Engine, mut ctx: Ctx, path: &str) -> (Ctx, Result<String, Error>) {
    let mut path = String::from(path);
    loop {
        let wasm = match load_module(&mut ctx.prog, &path) {
            Ok(w) => w,
            Err(e) => return (ctx, Err(Error::from(alloc::format!("{path}: {e}")))),
        };
        let platform = ctx.prog.kernel.platform.clone();
        platform.set_current_pid(ctx.prog.pid);
        let (back, res) = engine.run(&wasm, ctx);
        ctx = back;
        match res {
            Ok(Stop::Exit(status)) => return (ctx, Ok(status)),
            Ok(Stop::Exec { path: p, argv }) => {
                path = command_path(&p);
                ctx.argv = argv;
            }
            Err(e) => return (ctx, Err(Error::from(alloc::format!("{path}: {e}")))),
        }
    }
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
        $m!($crate::WASI, "clock_time_get", $crate::wasi::clock_time_get, (id: i32, precision: i64, time: i32) -> i32);
        $m!($crate::WASI, "random_get", $crate::wasi::random_get, (buf: i32, len: i32) -> i32);
        $m!($crate::WASI, "proc_exit", $crate::wasi::proc_exit, (code: i32) -> ());
        $m!($crate::PURGATORIO, "errstr", $crate::purgatorio::errstr, (buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "werrstr", $crate::purgatorio::werrstr, (ptr: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "exits", $crate::purgatorio::exits, (ptr: i32, len: i32) -> ());
        $m!($crate::PURGATORIO, "stat", $crate::purgatorio::stat, (path: i32, plen: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "fstat", $crate::purgatorio::fstat, (fd: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "dirread", $crate::purgatorio::dirread, (fd: i32, buf: i32, len: i32) -> i32);
        $m!($crate::PURGATORIO, "exec", $crate::purgatorio::exec, (path: i32, plen: i32, argv: i32, alen: i32) -> i32);
    };
}

#[doc(hidden)]
pub const WASI: &str = purg_abi::wasi::MODULE;
#[doc(hidden)]
pub const PURGATORIO: &str = purg_abi::MODULE;
