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

/// `std::thread::sleep` for both widths.
pub fn sleep_for(d: Duration) {
    sleep(u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
}

pub use std::time::Duration;

#[cfg(not(target_arch = "wasm64"))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_arch = "wasm64")]
pub use wide::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

/// `std::time`'s clocks for a wasm64 guest, where `std`'s own panic.
#[cfg(target_arch = "wasm64")]
mod wide {
    use std::fmt;
    use std::ops::{Add, AddAssign, Sub, SubAssign};
    use std::time::Duration;

    /// A time on the wall clock, as `std::time::SystemTime`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct SystemTime(Duration);

    /// 1970-01-01 00:00 UTC.
    pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::ZERO);

    /// The second time was later than the first; holds by how much.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SystemTimeError(Duration);

    impl SystemTimeError {
        pub fn duration(&self) -> Duration {
            self.0
        }
    }

    impl fmt::Display for SystemTimeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("second time provided was later than self")
        }
    }

    impl std::error::Error for SystemTimeError {}

    impl SystemTime {
        pub const UNIX_EPOCH: SystemTime = UNIX_EPOCH;

        pub fn now() -> SystemTime {
            SystemTime(Duration::from_nanos(super::nsec()))
        }

        pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
            self.0
                .checked_sub(earlier.0)
                .ok_or(SystemTimeError(earlier.0.saturating_sub(self.0)))
        }

        pub fn elapsed(&self) -> Result<Duration, SystemTimeError> {
            SystemTime::now().duration_since(*self)
        }

        pub fn checked_add(&self, d: Duration) -> Option<SystemTime> {
            self.0.checked_add(d).map(SystemTime)
        }

        pub fn checked_sub(&self, d: Duration) -> Option<SystemTime> {
            self.0.checked_sub(d).map(SystemTime)
        }
    }

    impl Add<Duration> for SystemTime {
        type Output = SystemTime;
        fn add(self, d: Duration) -> SystemTime {
            SystemTime(self.0 + d)
        }
    }

    impl Sub<Duration> for SystemTime {
        type Output = SystemTime;
        fn sub(self, d: Duration) -> SystemTime {
            SystemTime(self.0 - d)
        }
    }

    /// A time on the monotonic clock, as `std::time::Instant`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(Duration);

    impl Instant {
        pub fn now() -> Instant {
            Instant(Duration::from_nanos(super::mono()))
        }

        pub fn elapsed(&self) -> Duration {
            Instant::now().saturating_duration_since(*self)
        }

        /// As `std`'s: zero if `earlier` is later.
        pub fn duration_since(&self, earlier: Instant) -> Duration {
            self.saturating_duration_since(earlier)
        }

        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_sub(earlier.0)
        }

        pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
            self.0.checked_sub(earlier.0)
        }

        pub fn checked_add(&self, d: Duration) -> Option<Instant> {
            self.0.checked_add(d).map(Instant)
        }

        pub fn checked_sub(&self, d: Duration) -> Option<Instant> {
            self.0.checked_sub(d).map(Instant)
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            Instant(self.0 + d)
        }
    }

    impl AddAssign<Duration> for Instant {
        fn add_assign(&mut self, d: Duration) {
            self.0 += d;
        }
    }

    impl Sub<Duration> for Instant {
        type Output = Instant;
        fn sub(self, d: Duration) -> Instant {
            Instant(self.0 - d)
        }
    }

    impl SubAssign<Duration> for Instant {
        fn sub_assign(&mut self, d: Duration) {
            self.0 -= d;
        }
    }

    impl Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, earlier: Instant) -> Duration {
            self.saturating_duration_since(earlier)
        }
    }
}
