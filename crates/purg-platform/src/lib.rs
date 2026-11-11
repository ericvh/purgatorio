//! The seam between the portable core and whatever runs it: the hosted emu
//! (`purg-emu`), the bare-metal probe (`purg-probe`) and, later, the aarch64
//! kernel. Everything `purg-core` needs from its environment goes through
//! [`Platform`], and every lock it takes goes through [`sync`].
#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod idle;
pub mod net;
pub mod sync;

use alloc::borrow::Cow;
use alloc::boxed::Box;
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
    /// Turns the host terminal's raw mode on or off. Devices that are already
    /// byte streams (a UART, a pipe) do nothing.
    fn set_raw(&self, _on: bool) -> Result<()> {
        Ok(())
    }
    /// True if `#c` must echo and edit the input itself: this is a bare
    /// byte stream (a UART) with no terminal driver in front of it. False
    /// on hosted, where the host terminal does both.
    fn line_discipline(&self) -> bool {
        false
    }
}

/// A battery-backed real-time clock, served as `#r/rtc`.
pub trait Rtc: Send + Sync {
    /// The clock's time in seconds since the Unix epoch.
    fn seconds(&self) -> u64;
    /// Sets the clock. Clocks that cannot be set fail with
    /// `"permission denied"`.
    fn set_seconds(&self, secs: u64) -> Result<()>;
}

/// One line of `#c/memory`: an allocation pool, with the fields of
/// Inferno's `Pool` that the file prints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemPool {
    pub name: &'static str,
    /// Bytes in use.
    pub cursize: u64,
    /// The most the pool can hold.
    pub maxsize: u64,
    /// The high-water mark of `cursize`.
    pub hw: u64,
    /// Allocations and frees so far.
    pub nalloc: u64,
    pub nfree: u64,
    /// Times the pool grew.
    pub nbrk: u64,
    /// The largest block that could be allocated now.
    pub max: u64,
}

/// Services the portable core needs from its environment. Sleep and wakeup
/// go through [`sync::Rendez`].
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
    /// Starts a kernel process (Inferno's `kproc`) running `f`: an OS thread
    /// on hosted, a kernel thread on bare metal. Fails with `"no free
    /// procs"` when none can be created.
    fn spawn_kproc(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> Result<()>;
    /// Bytes of memory in use and bytes available to the system, for `#c/memory`
    /// and `#%`. `(0, 0)` means the platform has no figures.
    fn memory(&self) -> (u64, u64) {
        (0, 0)
    }
    /// The pools `#c/memory` lists. Platforms that only have
    /// [`Platform::memory`]'s figures report them as one `main` pool.
    fn pools(&self) -> alloc::vec::Vec<MemPool> {
        let (used, total) = self.memory();
        alloc::vec![MemPool {
            name: "main",
            cursize: used,
            maxsize: total,
            hw: used,
            max: total.saturating_sub(used),
            ..MemPool::default()
        }]
    }
    /// Records which prog's [`sync::Note`] `kill` should interrupt on this
    /// thread. The hosted emu keeps it thread-local; bare metal keeps it on
    /// the current kernel thread.
    fn set_note(&self, note: Option<alloc::sync::Arc<sync::Note>>) {
        sync::set_current_note(note);
    }
    /// The network stack, if this platform has one. `#I` fails with
    /// `"network unreachable"` when this is `None`.
    fn net(&self) -> Option<&dyn net::Net> {
        None
    }
    /// Starts a host command for `#C`. Platforms without a host shell fail
    /// with `"not implemented"`.
    fn command(&self, _line: &str) -> Result<alloc::sync::Arc<dyn net::Command>> {
        Err(Cow::Borrowed("not implemented"))
    }
}
