//! Clocks and sleeping for both widths.
//!
//! A wasm32 guest has `std::time` and `std::thread::sleep` through WASI.
//! In a wasm64 guest those panic: there is no WASI to ask
//! (`docs/wasm64.md`). These work in both.

/// Nanoseconds since 1970 UTC.
#[cfg(not(target_arch = "wasm64"))]
pub fn nsec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Nanoseconds since 1970 UTC.
#[cfg(target_arch = "wasm64")]
pub fn nsec() -> u64 {
    unsafe { crate::sys::nsec() }.max(0) as u64
}

/// Seconds since 1970 UTC.
pub fn now() -> u64 {
    nsec() / 1_000_000_000
}

/// Nanoseconds on a clock that only goes forward, from an arbitrary
/// start.
#[cfg(not(target_arch = "wasm64"))]
pub fn mono() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Nanoseconds on a clock that only goes forward, from an arbitrary
/// start.
#[cfg(target_arch = "wasm64")]
pub fn mono() -> u64 {
    unsafe { crate::sys::mono() }.max(0) as u64
}

/// `sys->sleep`: waits `ms` milliseconds.
#[cfg(not(target_arch = "wasm64"))]
pub fn sleep(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(ms.into()));
}

/// `sys->sleep`: waits `ms` milliseconds.
#[cfg(target_arch = "wasm64")]
pub fn sleep(ms: u32) {
    unsafe { crate::sys::sleep(i32::try_from(ms).unwrap_or(i32::MAX)) };
}
