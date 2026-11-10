//! A minimal `no_std` runtime for Purgatorio guests on
//! `wasm32-unknown-unknown`: the experiment behind the `no_std` guest
//! profile in docs/footprint.md. It is what a `no_std` mode of the SDK
//! would start from, cut down to what `echo`, `cat` and `sleep` need:
//!
//! - the entry point and the `purgatorio.abi` section ([`entry!`]);
//! - the arguments, read with `args_sizes_get` and `args_get` into memory
//!   from a bump allocator that is never freed ([`args`]);
//! - `read`, `write`, `open`, `close`, `errstr`, `exits` and `sleep_ms`
//!   over the same imports the `std` guests end up calling;
//! - a panic handler that exits with status `panic`.
//!
//! There is no heap allocator, no formatting and no `String`: a command
//! that needs them pays for them.
#![no_std]

use core::arch::wasm32;

pub use purg_abi;

mod sys {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        pub fn args_sizes_get(argc: *mut u32, size: *mut u32) -> i32;
        pub fn args_get(argv: *mut *const u8, buf: *mut u8) -> i32;
        pub fn fd_read(fd: i32, iovs: *const Iovec, n: usize, nread: *mut usize) -> i32;
        pub fn fd_write(fd: i32, iovs: *const Iovec, n: usize, nwritten: *mut usize) -> i32;
        pub fn fd_close(fd: i32) -> i32;
        pub fn poll_oneoff(subs: *const u8, events: *mut u8, n: usize, nevents: *mut usize) -> i32;
    }

    #[link(wasm_import_module = "purgatorio")]
    unsafe extern "C" {
        pub fn open(path: *const u8, plen: usize, mode: i32) -> i32;
        pub fn errstr(buf: *mut u8, len: usize) -> i32;
        pub fn exits(status: *const u8, len: usize) -> !;
    }

    #[repr(C)]
    pub struct Iovec {
        pub buf: *const u8,
        pub len: usize,
    }
}

/// Defines `_start` calling `$main` (a `fn()`), and the `purgatorio.abi`
/// custom section. A `main` that returns exits with the empty status.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[used]
        #[unsafe(link_section = "purgatorio.abi")]
        static __PURGATORIO_ABI: [u8; $crate::ABI_LEN] = $crate::ABI_BYTES;

        #[unsafe(no_mangle)]
        pub extern "C" fn _start() {
            $main();
        }
    };
}

pub const ABI_LEN: usize = purg_abi::ABI_VERSION.len();

pub const ABI_BYTES: [u8; ABI_LEN] = {
    let src = purg_abi::ABI_VERSION.as_bytes();
    let mut out = [0u8; ABI_LEN];
    let mut i = 0;
    while i < ABI_LEN {
        out[i] = src[i];
        i += 1;
    }
    out
};

pub const STDIN: i32 = 0;
pub const STDOUT: i32 = 1;
pub const STDERR: i32 = 2;

const PAGE: usize = 65536;

unsafe extern "C" {
    /// The end of the module's data and stack, from the linker.
    static __heap_base: u8;
}

/// The next free byte; 0 until the first allocation.
static mut BRK: usize = 0;

/// `len` bytes aligned to `align` (a power of two) that are never freed,
/// growing the memory when the current pages run out. `None` when the
/// system has no more memory.
pub fn bump(len: usize, align: usize) -> Option<&'static mut [u8]> {
    // SAFETY: a guest has one thread, so nothing else touches `BRK`.
    let brk = unsafe {
        if BRK == 0 {
            BRK = &raw const __heap_base as usize;
        }
        BRK
    };
    let start = brk.checked_next_multiple_of(align)?;
    let end = start.checked_add(len)?;
    let have = wasm32::memory_size(0) * PAGE;
    if end > have && wasm32::memory_grow(0, (end - have).div_ceil(PAGE)) == usize::MAX {
        return None;
    }
    // SAFETY: as above; the range is inside the memory and handed out once.
    unsafe {
        BRK = end;
        Some(core::slice::from_raw_parts_mut(start as *mut u8, len))
    }
}

/// The arguments, each a byte string without its NUL.
#[derive(Clone, Copy)]
pub struct Args {
    ptrs: &'static [*const u8],
    end: *const u8,
}

impl Args {
    pub fn len(&self) -> usize {
        self.ptrs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ptrs.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&'static [u8]> {
        let start = *self.ptrs.get(i)?;
        let next = self.ptrs.get(i + 1).copied().unwrap_or(self.end);
        // SAFETY: `args_get` filled the buffer with NUL-terminated strings
        // in order, so each runs to one byte before the next.
        Some(unsafe {
            core::slice::from_raw_parts(start, (next as usize - start as usize).saturating_sub(1))
        })
    }
}

/// Reads the arguments. Exits with `fail:no memory` if they do not fit.
pub fn args() -> Args {
    let (mut argc, mut size) = (0u32, 0u32);
    // SAFETY: the pointers are to locals of the right type.
    unsafe { sys::args_sizes_get(&mut argc, &mut size) };
    let (argc, size) = (argc as usize, size as usize);
    let (Some(ptrs), Some(buf)) = (
        bump(argc * size_of::<*const u8>(), align_of::<*const u8>()),
        bump(size, 1),
    ) else {
        exits("fail:no memory")
    };
    let ptrs = ptrs.as_mut_ptr().cast::<*const u8>();
    // SAFETY: the two blocks are the sizes `args_sizes_get` asked for, and
    // the pointer block is aligned for pointers.
    unsafe {
        sys::args_get(ptrs, buf.as_mut_ptr());
        Args {
            ptrs: core::slice::from_raw_parts(ptrs, argc),
            end: buf.as_ptr().add(size),
        }
    }
}

/// Reads up to `buf.len()` bytes; `None` on an error (see [`errstr`]).
pub fn read(fd: i32, buf: &mut [u8]) -> Option<usize> {
    let iov = sys::Iovec {
        buf: buf.as_mut_ptr(),
        len: buf.len(),
    };
    let mut n = 0;
    // SAFETY: one iovec over `buf`, and a local for the count.
    (unsafe { sys::fd_read(fd, &iov, 1, &mut n) } == 0).then_some(n)
}

/// Writes all of `buf`; `false` on an error.
pub fn write(fd: i32, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let iov = sys::Iovec {
            buf: buf.as_ptr(),
            len: buf.len(),
        };
        let mut n = 0;
        // SAFETY: one iovec over `buf`, and a local for the count.
        if unsafe { sys::fd_write(fd, &iov, 1, &mut n) } != 0 || n == 0 {
            return false;
        }
        buf = &buf[n.min(buf.len())..];
    }
    true
}

/// Opens `path` for reading; `None` on an error.
pub fn open_read(path: &[u8]) -> Option<i32> {
    // SAFETY: the pointer and length describe `path`.
    let fd = unsafe { sys::open(path.as_ptr(), path.len(), i32::from(purg_abi::mode::OREAD)) };
    (fd >= 0).then_some(fd)
}

pub fn close(fd: i32) {
    // SAFETY: takes no pointers.
    unsafe { sys::fd_close(fd) };
}

/// The prog's error string, copied into `buf`.
pub fn errstr(buf: &mut [u8]) -> &[u8] {
    // SAFETY: the pointer and length describe `buf`.
    let n = unsafe { sys::errstr(buf.as_mut_ptr(), buf.len()) };
    &buf[..usize::try_from(n).unwrap_or(0).min(buf.len())]
}

/// Ends the prog with `status`; the empty status is success.
pub fn exits(status: &str) -> ! {
    // SAFETY: the pointer and length describe `status`.
    unsafe { sys::exits(status.as_ptr(), status.len()) }
}

/// Sleeps for `ns` nanoseconds, with a WASI clock subscription, as `std`'s
/// `thread::sleep` does.
pub fn sleep_ns(ns: u64) {
    // A `subscription`: userdata at 0, the tag (0 is a clock) at 8, the
    // clock id (1 is monotonic) at 16, the timeout at 24, the precision
    // at 32 and the flags (0 is relative) at 40.
    let mut sub = [0u64; 6];
    sub[2] = 1;
    sub[3] = ns;
    let mut event = [0u64; 4];
    let mut n = 0;
    // SAFETY: one 48-byte subscription, room for one 32-byte event.
    unsafe { sys::poll_oneoff(sub.as_ptr().cast(), event.as_mut_ptr().cast(), 1, &mut n) };
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    exits("panic")
}
