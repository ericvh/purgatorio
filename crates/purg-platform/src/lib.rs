//! The seam between the portable core and whatever runs it: the hosted emu
//! (`purg-emu`), the bare-metal probe (`purg-probe`) and, later, the aarch64
//! kernel. Everything `purg-core` needs from its environment goes through
//! [`Platform`], and every lock it takes goes through [`sync`].
#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod sync;

use alloc::borrow::Cow;
use alloc::string::String;

/// Platform errors carry Inferno-style error strings.
pub type Result<T> = core::result::Result<T, Cow<'static, str>>;

/// Raw console byte stream. On hosted this is the emu's stdin and stdout; on
/// bare metal it is a UART.
pub trait Console: Send + Sync {
    /// Reads raw bytes, blocking until at least one is available.
    /// Returns 0 at end of input.
    fn read(&self, buf: &mut [u8]) -> Result<usize>;
    /// Writes all of `buf`.
    fn write(&self, buf: &[u8]) -> Result<usize>;
}

/// Services the portable core needs from its environment.
///
/// Later milestones add `spawn_kproc` and the `Rendez` sleep/wakeup
/// primitive here; M1 runs one prog on the calling thread.
pub trait Platform: Send + Sync {
    /// Wall-clock time in nanoseconds since the Unix epoch.
    fn now_ns(&self) -> u64;
    /// Monotonic time in nanoseconds since an arbitrary fixed point.
    fn monotonic_ns(&self) -> u64;
    /// Fills `buf` with random bytes.
    fn entropy(&self, buf: &mut [u8]);
    fn console(&self) -> &dyn Console;
    /// The user name reported by `#c/user`.
    fn user(&self) -> String;
    /// The system name reported by `#c/sysname`.
    fn sysname(&self) -> String;
    /// Pid of the prog running on the calling kproc (Inferno's `up->pid`).
    /// Hosted keeps this thread-local.
    fn current_pid(&self) -> u32;
    fn set_current_pid(&self, pid: u32);
}
