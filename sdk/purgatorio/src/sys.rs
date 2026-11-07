//! Raw `purgatorio` imports. Prefer the safe wrappers in the crate root.
//!
//! On non-WASI targets (host builds of app crates, used by `cargo check` and
//! clippy) these are stubs that fail with "not implemented".

#[cfg(target_os = "wasi")]
#[link(wasm_import_module = "purgatorio")]
unsafe extern "C" {
    pub fn errstr(buf: *mut u8, len: usize) -> i32;
    pub fn werrstr(msg: *const u8, len: usize) -> i32;
    pub fn exits(status: *const u8, len: usize) -> !;
    pub fn stat(path: *const u8, plen: usize, buf: *mut u8, len: usize) -> i32;
    pub fn fstat(fd: i32, buf: *mut u8, len: usize) -> i32;
    pub fn dirread(fd: i32, buf: *mut u8, len: usize) -> i32;
    pub fn exec(path: *const u8, plen: usize, argv: *const u8, alen: usize) -> i32;
}

#[cfg(not(target_os = "wasi"))]
mod stubs {
    #![allow(clippy::missing_safety_doc)]
    const MSG: &str = purg_abi::error::ENOTIMPL;

    pub unsafe fn errstr(buf: *mut u8, len: usize) -> i32 {
        let n = MSG.len().min(len);
        // SAFETY: the caller provides `len` writable bytes at `buf`.
        unsafe { core::ptr::copy_nonoverlapping(MSG.as_ptr(), buf, n) };
        n as i32
    }
    pub unsafe fn werrstr(_msg: *const u8, _len: usize) -> i32 {
        -1
    }
    pub unsafe fn exits(_status: *const u8, len: usize) -> ! {
        std::process::exit(i32::from(len != 0))
    }
    pub unsafe fn stat(_path: *const u8, _plen: usize, _buf: *mut u8, _len: usize) -> i32 {
        -1
    }
    pub unsafe fn fstat(_fd: i32, _buf: *mut u8, _len: usize) -> i32 {
        -1
    }
    pub unsafe fn dirread(_fd: i32, _buf: *mut u8, _len: usize) -> i32 {
        -1
    }
    pub unsafe fn exec(_path: *const u8, _plen: usize, _argv: *const u8, _alen: usize) -> i32 {
        -1
    }
}

#[cfg(not(target_os = "wasi"))]
pub use stubs::*;
