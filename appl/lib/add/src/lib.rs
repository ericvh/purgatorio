//! Reactor loaded by `load add`. Export `add` reads `a b` from its argument
//! bytes and writes their sum back over those bytes.
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

/// `(args_ptr, args_len) -> result_len`. Arguments are ASCII `a b`.
#[unsafe(no_mangle)]
pub extern "C" fn add(ptr: i32, len: i32) -> i32 {
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    let text = core::str::from_utf8(input).unwrap_or("");
    let mut parts = text.split_whitespace();
    let a = parse(parts.next().unwrap_or("0"));
    let b = parse(parts.next().unwrap_or("0"));
    write_i64(ptr as *mut u8, a.wrapping_add(b))
}

fn parse(s: &str) -> i64 {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let mut n = 0i64;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            return 0;
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
    }
    if neg { n.wrapping_neg() } else { n }
}

fn write_i64(dst: *mut u8, n: i64) -> i32 {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let neg = n < 0;
    let mut v = n.unsigned_abs();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    let n = buf.len() - i;
    unsafe { core::ptr::copy_nonoverlapping(buf[i..].as_ptr(), dst, n) };
    n as i32
}
