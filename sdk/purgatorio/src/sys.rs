//! Raw `purgatorio` imports. Prefer the safe wrappers in the crate root.
//!
//! On non-WASI targets (host builds of app crates, used by `cargo check` and
//! clippy) these are stubs that fail with "not implemented".

macro_rules! imports {
    ($(fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        #[cfg(target_os = "wasi")]
        #[link(wasm_import_module = "purgatorio")]
        unsafe extern "C" {
            $(pub fn $name($($arg: $ty),*) -> $ret;)*
        }

        #[cfg(not(target_os = "wasi"))]
        mod stubs {
            #![allow(clippy::missing_safety_doc)]
            $(
                pub unsafe fn $name($(_: $ty),*) -> $ret {
                    -1
                }
            )*
        }

        #[cfg(not(target_os = "wasi"))]
        pub use stubs::*;
    };
}

imports! {
    fn werrstr(msg: *const u8, len: usize) -> i32;
    fn stat(path: *const u8, plen: usize, buf: *mut u8, len: usize) -> i32;
    fn fstat(fd: i32, buf: *mut u8, len: usize) -> i32;
    fn dirread(fd: i32, buf: *mut u8, len: usize) -> i32;
    fn open(path: *const u8, plen: usize, mode: i32) -> i32;
    fn create(path: *const u8, plen: usize, mode: i32, perm: u32) -> i32;
    fn wstat(path: *const u8, plen: usize, dir: *const u8, dlen: usize) -> i32;
    fn fwstat(fd: i32, dir: *const u8, dlen: usize) -> i32;
    fn remove(path: *const u8, plen: usize) -> i32;
    fn fd2path(fd: i32, buf: *mut u8, len: usize) -> i32;
    fn chdir(path: *const u8, plen: usize) -> i32;
    fn dup(old: i32, new: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn iounit(fd: i32) -> i32;
    fn bind(name: *const u8, nlen: usize, old: *const u8, olen: usize, flags: u32) -> i32;
    fn unmount(name: *const u8, nlen: usize, old: *const u8, olen: usize) -> i32;
    fn pctl(flags: u32, movefd: *const i32, n: usize) -> i32;
    fn spawn_prog(
        path: *const u8,
        plen: usize,
        argv: *const u8,
        alen: usize,
        flags: u32,
        fdmap: *const i32,
        n: usize
    ) -> i32;
    fn mount(
        fd: i32,
        afd: i32,
        old: *const u8,
        olen: usize,
        flags: u32,
        aname: *const u8,
        alen: usize
    ) -> i32;
    fn export(fd: i32, dir: *const u8, dlen: usize, flags: u32) -> i32;
    fn export_key(
        fd: i32,
        dir: *const u8,
        dlen: usize,
        flags: u32,
        key: *const u8,
        klen: usize
    ) -> i32;
    fn fauth(fd: i32, aname: *const u8, alen: usize) -> i32;
    fn stream(src: i32, dst: i32, bufsiz: i32) -> i32;
    fn fversion(fd: i32, msize: i32, ver: *mut u8, vlen: usize) -> i32;
    fn file2chan(
        dir: *const u8,
        dlen: usize,
        name: *const u8,
        nlen: usize,
        handles: *mut i32
    ) -> i32;
    fn chan_new(cap: i32) -> i32;
    fn chan_close(h: i32) -> i32;
    fn chan_send(h: i32, buf: *const u8, n: usize, handles: *const i32, nh: usize) -> i32;
    fn chan_recv(h: i32, buf: *mut u8, blen: usize, handles: *mut i32, maxh: usize) -> i32;
    fn alt(ops: *mut i32, nops: usize, block: i32) -> i32;
    fn wait(ops: *const i32, nops: usize, timeout_ms: i32) -> i32;
    fn load(path: *const u8, plen: usize) -> i32;
    fn call(
        handle: i32,
        name: *const u8,
        nlen: usize,
        args: *const u8,
        alen: usize,
        out: *mut u8,
        olen: usize
    ) -> i32;
}

#[cfg(target_os = "wasi")]
#[link(wasm_import_module = "purgatorio")]
unsafe extern "C" {
    pub fn errstr(buf: *mut u8, len: usize) -> i32;
    pub fn exits(status: *const u8, len: usize) -> !;
}

#[cfg(not(target_os = "wasi"))]
mod host {
    #![allow(clippy::missing_safety_doc)]
    const MSG: &str = purg_abi::error::ENOTIMPL;

    pub unsafe fn errstr(buf: *mut u8, len: usize) -> i32 {
        let n = MSG.len().min(len);
        // SAFETY: the caller provides `len` writable bytes at `buf`.
        unsafe { core::ptr::copy_nonoverlapping(MSG.as_ptr(), buf, n) };
        n as i32
    }
    pub unsafe fn exits(_status: *const u8, len: usize) -> ! {
        std::process::exit(i32::from(len != 0))
    }
}

#[cfg(not(target_os = "wasi"))]
pub use host::{errstr, exits};
