//! A builtin module for mash, loaded with `load mashcase`. It defines
//! `upper` and `lower`, which print their arguments in upper or lower
//! case.
//!
//! The module interface: `mashinit` gets the `load` command line and
//! returns the names of the builtins, separated by spaces. Each builtin
//! is an export of its name that gets the command's words, each ended by
//! a NUL byte, and returns what the command prints. Results overwrite the
//! argument page, as for any reactor.
//!
//! `no_std` only on wasm. A host `clippy --workspace` builds this cdylib
//! with unwinding panics, which a `no_std` crate cannot do.
#![cfg_attr(target_arch = "wasm32", no_std)]

// Only wasm has this section; Mach-O rejects a section name with no segment.
#[used]
#[cfg_attr(target_family = "wasm", unsafe(link_section = "purgatorio.abi"))]
static ABI: [u8; 4] = *b"0.10";

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

const NAMES: &[u8] = b"upper lower";

/// The bytes at `(ptr, len)`, which the host has filled and which the
/// export may overwrite.
fn page(ptr: i32, len: i32) -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len.max(0) as usize) }
}

#[unsafe(no_mangle)]
pub extern "C" fn mashinit(ptr: i32, _len: i32) -> i32 {
    page(ptr, NAMES.len() as i32).copy_from_slice(NAMES);
    NAMES.len() as i32
}

/// Rewrites the words after the first in place: mapped by `f`, joined by
/// spaces, with a newline.
fn convert(ptr: i32, len: i32, f: fn(u8) -> u8) -> i32 {
    let buf = page(ptr, len);
    let n = buf.len();
    let start = buf.iter().position(|&b| b == 0).map_or(n, |i| i + 1);
    let mut out = 0;
    let mut i = start;
    while i < n {
        let b = buf[i];
        buf[out] = if b == 0 {
            if i + 1 == n { b'\n' } else { b' ' }
        } else {
            f(b)
        };
        out += 1;
        i += 1;
    }
    out as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn upper(ptr: i32, len: i32) -> i32 {
    convert(ptr, len, |b| b.to_ascii_uppercase())
}

#[unsafe(no_mangle)]
pub extern "C" fn lower(ptr: i32, len: i32) -> i32 {
    convert(ptr, len, |b| b.to_ascii_lowercase())
}
