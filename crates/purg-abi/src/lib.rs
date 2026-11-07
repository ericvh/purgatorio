//! The Purgatorio application ABI, shared by the host (`purg-vm`) and guests
//! (`sdk/purgatorio`).
//!
//! Apps are WASI preview1 command modules. They import the subset of
//! [`wasi`] that maps exactly onto the Inferno Sys API, plus the [`MODULE`]
//! (`"purgatorio"`) extension module for everything WASI cannot express
//! faithfully (string errors and exit status, full `Dir`s, namespaces, ...).
//! `docs/abi.md` has the full Sys-to-ABI mapping.
//!
//! Conventions: pointers are `u32` offsets into the guest's exported `memory`;
//! strings are `(ptr, len)` UTF-8. WASI calls return an errno; `purgatorio`
//! calls return `-1`. Either way the exact Inferno error string is recorded as
//! the prog's errstr.
#![no_std]

/// ABI version, carried by guests in the [`ABI_SECTION`] custom section.
pub const ABI_VERSION: &str = "0.2";
/// Name of the custom section holding [`ABI_VERSION`].
pub const ABI_SECTION: &str = "purgatorio.abi";

/// The Purgatorio extension import module.
pub const MODULE: &str = "purgatorio";
/// Guest export called to start a command (WASI command entry point).
pub const ENTRY: &str = "_start";
/// Guest export holding linear memory.
pub const MEMORY: &str = "memory";

/// Fd preopened as the prog's namespace root.
pub const PREOPEN_ROOT_FD: i32 = 3;
/// Fd preopened as the prog's current directory ("dot"); it always follows
/// the prog's current dot rather than a fixed directory.
pub const PREOPEN_DOT_FD: i32 = 4;

/// Imports in the `purgatorio` module implemented in this ABI version.
pub mod sys {
    /// `errstr(buf, blen) -> n`
    pub const ERRSTR: &str = "errstr";
    /// `werrstr(ptr, len) -> 0`
    pub const WERRSTR: &str = "werrstr";
    /// `exits(status, slen) -> !`
    pub const EXITS: &str = "exits";
    /// `stat(path, plen, buf, blen) -> n` (9P stat wire format)
    pub const STAT: &str = "stat";
    /// `fstat(fd, buf, blen) -> n` (9P stat wire format)
    pub const FSTAT: &str = "fstat";
    /// `dirread(fd, buf, blen) -> n` (concatenated 9P stat entries)
    pub const DIRREAD: &str = "dirread";
    /// `open(path, plen, mode) -> fd` with Inferno mode bits
    pub const OPEN: &str = "open";
    /// `create(path, plen, mode, perm) -> fd`
    pub const CREATE: &str = "create";
    /// `wstat(path, plen, dir, dlen) -> 0`; null fields mean "don't change"
    pub const WSTAT: &str = "wstat";
    /// `fwstat(fd, dir, dlen) -> 0`
    pub const FWSTAT: &str = "fwstat";
    /// `remove(path, plen) -> 0`
    pub const REMOVE: &str = "remove";
    /// `fd2path(fd, buf, blen) -> n`
    pub const FD2PATH: &str = "fd2path";
    /// `chdir(path, plen) -> 0`; moves the live `.` preopen
    pub const CHDIR: &str = "chdir";
    /// `dup(old, new) -> fd`; `new = -1` picks the lowest free fd
    pub const DUP: &str = "dup";
    /// `pipe(fds) -> 0`; writes two `i32` fds
    pub const PIPE: &str = "pipe";
    /// `iounit(fd) -> n`
    pub const IOUNIT: &str = "iounit";
    /// `bind(name, nlen, old, olen, flags) -> 0`
    pub const BIND: &str = "bind";
    /// `unmount(name, nlen, old, olen) -> 0`; `nlen = 0` unmounts everything
    /// on `old`
    pub const UNMOUNT: &str = "unmount";
    /// `pctl(flags, movefd, n) -> pid`; `movefd` holds `n` `i32` fds kept by
    /// `NEWFD`
    pub const PCTL: &str = "pctl";
    /// `spawn_prog(path, plen, argv, alen, flags, fdmap, n) -> pid`. `argv` is
    /// NUL-terminated strings back to back; `fdmap` holds `n` pairs of `i32`
    /// `(parent fd, child fd)` installed in the child after `flags` apply.
    pub const SPAWN_PROG: &str = "spawn_prog";
}

/// `pctl` and `spawn_prog` flags (`Sys->NEWFD` ...).
pub mod pctl {
    pub const NEWFD: u32 = 1 << 0;
    pub const FORKFD: u32 = 1 << 1;
    pub const NEWNS: u32 = 1 << 2;
    pub const FORKNS: u32 = 1 << 3;
    pub const NEWPGRP: u32 = 1 << 4;
    pub const NODEVS: u32 = 1 << 5;
    pub const NEWENV: u32 = 1 << 6;
    pub const FORKENV: u32 = 1 << 7;
}

/// WASI preview1 names, constants and errnos.
pub mod wasi {
    pub const MODULE: &str = "wasi_snapshot_preview1";

    pub const CLOCKID_REALTIME: u32 = 0;
    pub const CLOCKID_MONOTONIC: u32 = 1;

    pub const FILETYPE_UNKNOWN: u8 = 0;
    pub const FILETYPE_CHARACTER_DEVICE: u8 = 2;
    pub const FILETYPE_DIRECTORY: u8 = 3;
    pub const FILETYPE_REGULAR_FILE: u8 = 4;

    pub const OFLAGS_CREAT: u16 = 1;
    pub const OFLAGS_DIRECTORY: u16 = 2;
    pub const OFLAGS_EXCL: u16 = 4;
    pub const OFLAGS_TRUNC: u16 = 8;

    pub const FDFLAGS_APPEND: u16 = 1;

    pub const LOOKUPFLAGS_SYMLINK_FOLLOW: u32 = 1;

    pub const FSTFLAGS_ATIM: u16 = 1;
    pub const FSTFLAGS_ATIM_NOW: u16 = 2;
    pub const FSTFLAGS_MTIM: u16 = 4;
    pub const FSTFLAGS_MTIM_NOW: u16 = 8;

    pub const EVENTTYPE_CLOCK: u8 = 0;
    pub const EVENTTYPE_FD_READ: u8 = 1;
    pub const EVENTTYPE_FD_WRITE: u8 = 2;
    pub const SUBCLOCKFLAGS_ABSTIME: u16 = 1;

    pub const RIGHTS_FD_READ: u64 = 1 << 1;
    pub const RIGHTS_FD_WRITE: u64 = 1 << 6;

    pub const WHENCE_SET: u8 = 0;
    pub const WHENCE_CUR: u8 = 1;
    pub const WHENCE_END: u8 = 2;

    pub const PREOPENTYPE_DIR: u8 = 0;

    /// WASI errno values.
    pub mod errno {
        pub const SUCCESS: i32 = 0;
        pub const ACCES: i32 = 2;
        pub const BADF: i32 = 8;
        pub const EXIST: i32 = 20;
        pub const FAULT: i32 = 21;
        pub const INTR: i32 = 27;
        pub const INVAL: i32 = 28;
        pub const IO: i32 = 29;
        pub const ISDIR: i32 = 31;
        pub const NOENT: i32 = 44;
        pub const NOSPC: i32 = 51;
        pub const NOSYS: i32 = 52;
        pub const NOTDIR: i32 = 54;
        pub const NOTEMPTY: i32 = 55;
        pub const NOTSUP: i32 = 58;
        pub const PIPE: i32 = 64;
        pub const SPIPE: i32 = 70;
        pub const XDEV: i32 = 75;
    }
}

/// Open modes (`Sys->OREAD` ...).
pub mod mode {
    pub const OREAD: u8 = 0;
    pub const OWRITE: u8 = 1;
    pub const ORDWR: u8 = 2;
    pub const OEXEC: u8 = 3;
    pub const OTRUNC: u8 = 0x10;
    pub const ORCLOSE: u8 = 0x40;
    /// Mask selecting the access part of a mode.
    pub const ACCESS: u8 = 3;
}

/// Bind and mount flags (`Sys->MREPL` ...).
pub mod mount {
    pub const MREPL: u32 = 0x0000;
    pub const MBEFORE: u32 = 0x0001;
    pub const MAFTER: u32 = 0x0002;
    pub const MCREATE: u32 = 0x0004;
    /// Mask selecting the ordering part of the flags.
    pub const MORDER: u32 = 0x0003;
}

/// Error strings, worded exactly as Inferno's.
pub mod error {
    pub const ENONEXIST: &str = "file does not exist";
    pub const EPERM: &str = "permission denied";
    pub const EISDIR: &str = "file is a directory";
    pub const ENOTDIR: &str = "not a directory";
    pub const EBADFD: &str = "fd out of range or not open";
    pub const EBADUSEFD: &str = "inappropriate use of fd";
    pub const EBADARG: &str = "bad arg in system call";
    pub const EBADSHARP: &str = "unknown device in # filename";
    pub const EFILENAME: &str = "file name syntax";
    pub const EEXIST: &str = "file already exists";
    pub const ENOFD: &str = "no free file descriptors";
    pub const EINTR: &str = "interrupted";
    pub const EHUNGUP: &str = "i/o on hungup channel";
    pub const EIO: &str = "i/o error";
    pub const ENOSPACE: &str = "file system full";
    pub const ESHORTSTAT: &str = "stat buffer too small";
    pub const EBADADDR: &str = "bad address";
    pub const ENOTIMPL: &str = "not implemented";
    pub const EBADEXEC: &str = "bad executable";
    pub const ENOTSEEK: &str = "seek on a stream";
    pub const ENOTEMPTY: &str = "directory not empty";
    pub const ENOCREATE: &str = "mounted directory forbids creation";
    pub const ENOATTACH: &str = "mount/attach disallowed";
    pub const EUNMOUNT: &str = "not mounted";
    pub const EISMOUNT: &str = "inconsistent mount";
    pub const ENOPROC: &str = "no free procs";
    pub const EINUSE: &str = "device or object already in use";
    pub const EXDEV: &str = "rename across directories";
}

/// Maps an Inferno error string to the closest WASI errno. Strings without a
/// specific mapping (for example from a remote Styx server) map to `EIO`.
pub fn errno_for(err: &str) -> i32 {
    use error::*;
    use wasi::errno;
    match err {
        ENONEXIST => errno::NOENT,
        EPERM => errno::ACCES,
        EINTR => errno::INTR,
        EHUNGUP => errno::PIPE,
        EEXIST => errno::EXIST,
        ENOTDIR => errno::NOTDIR,
        EISDIR => errno::ISDIR,
        EBADFD | EBADUSEFD => errno::BADF,
        EBADADDR => errno::FAULT,
        EBADARG | EFILENAME | EBADSHARP => errno::INVAL,
        ENOSPACE => errno::NOSPC,
        ENOTIMPL => errno::NOTSUP,
        ENOTSEEK => errno::SPIPE,
        ENOTEMPTY => errno::NOTEMPTY,
        EXDEV => errno::XDEV,
        _ => errno::IO,
    }
}

/// Size in bytes of a 9P stat entry's fixed part, including its size field and
/// the four string length prefixes.
pub const STATFIXLEN: usize = 49;
