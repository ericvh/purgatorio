//! `std::thread::sleep` for both widths: in a wasm32 guest it is `std`'s
//! own, and in a wasm64 guest, whose `std` cannot sleep, the SDK's
//! (`docs/wasm64.md`).

#[cfg(not(target_arch = "wasm64"))]
pub use std::thread::sleep;

/// Waits for `d`.
#[cfg(target_arch = "wasm64")]
pub fn sleep(d: crate::time::Duration) {
    crate::time::sleep_for(d);
}
