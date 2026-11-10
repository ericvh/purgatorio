//! [`Platform`] on bare metal, for the kernel and the probe: a console,
//! the PL031 wall clock, the virtual counter, kprocs as threads of a
//! [`Scheduler`], and optionally a network stack.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use purg_platform::net::Net;
use purg_platform::{Console, Platform, Result, Rtc};

use crate::pl031::Pl031;
use crate::sched::Scheduler;
use crate::timer;

pub struct MetalPlatform {
    console: &'static dyn Console,
    sched: &'static Scheduler,
    sysname: &'static str,
    net: Option<Box<dyn Net>>,
    boot_epoch_ns: u64,
    boot_counter: u64,
    /// splitmix64 state for [`Platform::entropy`].
    seed: AtomicU64,
}

/// Seconds since the Unix epoch from the PL031 at physical `pa`.
///
/// # Safety
/// `pa` must be a PL031 in the device window the high half maps.
pub unsafe fn pl031_seconds(pa: usize) -> u64 {
    // SAFETY: the caller guarantees a mapped PL031.
    unsafe { Pl031::new(pa) }.seconds()
}

impl MetalPlatform {
    /// `rtc` is the PL031's physical address from the DTB; without one the
    /// wall clock starts at the epoch.
    pub fn new(
        console: &'static dyn Console,
        sched: &'static Scheduler,
        sysname: &'static str,
        rtc: Option<usize>,
    ) -> MetalPlatform {
        let boot_counter = timer::counter();
        // SAFETY: the DTB names a PL031 there, in the device window.
        let secs = rtc.map_or(0, |pa| unsafe { pl031_seconds(pa) });
        MetalPlatform {
            console,
            sched,
            sysname,
            net: None,
            boot_epoch_ns: secs * 1_000_000_000,
            boot_counter,
            seed: AtomicU64::new(boot_counter ^ 0x9e37_79b9_7f4a_7c15),
        }
    }

    /// Serves `#I` from `net`.
    pub fn with_net(mut self, net: Box<dyn Net>) -> MetalPlatform {
        self.net = Some(net);
        self
    }
}

impl Platform for MetalPlatform {
    fn now_ns(&self) -> u64 {
        self.boot_epoch_ns + self.monotonic_ns()
    }

    fn monotonic_ns(&self) -> u64 {
        timer::ticks_to_ns(timer::counter().wrapping_sub(self.boot_counter))
    }

    /// Not cryptographic: QEMU virt has no RNG without virtio-rng.
    fn entropy(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let mut z = self
                .seed
                .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
                .wrapping_add(timer::counter());
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
    }

    fn console(&self) -> &dyn Console {
        self.console
    }

    fn user(&self) -> String {
        String::from("inferno")
    }

    fn sysname(&self) -> String {
        String::from(self.sysname)
    }

    fn current_pid(&self) -> u32 {
        self.sched.current_pid()
    }

    fn set_current_pid(&self, pid: u32) {
        self.sched.set_current_pid(pid);
    }

    fn spawn_kproc(&self, _name: &str, f: Box<dyn FnOnce() + Send>) -> Result<()> {
        self.sched.spawn(f);
        Ok(())
    }

    fn net(&self) -> Option<&dyn Net> {
        self.net.as_deref()
    }
}
