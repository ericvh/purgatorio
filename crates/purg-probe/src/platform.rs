//! [`Platform`] for the probe: PL011 console, PL031 wall clock, generic
//! timer. One core, one prog at a time.

use alloc::string::String;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use purg_platform::{Console, Platform, Result};

use crate::hw;

struct Pl011;

impl Console for Pl011 {
    /// Never blocks: the probe's scripts take no console input, so an empty
    /// receive FIFO reads as end of input.
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        Ok(hw::uart_read(buf))
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        hw::uart_write(buf);
        Ok(buf.len())
    }
}

pub struct Probe {
    console: Pl011,
    boot_epoch_ns: u64,
    boot_counter: u64,
    hz: u64,
    pid: AtomicU32,
    /// splitmix64 state for [`Platform::entropy`].
    seed: AtomicU64,
}

impl Probe {
    pub fn new() -> Probe {
        let boot_counter = hw::counter();
        Probe {
            console: Pl011,
            boot_epoch_ns: hw::rtc_seconds() * 1_000_000_000,
            boot_counter,
            hz: hw::counter_hz().max(1),
            pid: AtomicU32::new(0),
            seed: AtomicU64::new(boot_counter ^ 0x9e37_79b9_7f4a_7c15),
        }
    }
}

impl Platform for Probe {
    fn now_ns(&self) -> u64 {
        self.boot_epoch_ns + self.monotonic_ns()
    }

    fn monotonic_ns(&self) -> u64 {
        let ticks = hw::counter().wrapping_sub(self.boot_counter);
        (u128::from(ticks) * 1_000_000_000 / u128::from(self.hz)) as u64
    }

    /// Not cryptographic: QEMU virt has no RNG without virtio-rng.
    fn entropy(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let s = self
                .seed
                .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
                .wrapping_add(hw::counter());
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
    }

    fn console(&self) -> &dyn Console {
        &self.console
    }

    fn user(&self) -> String {
        String::from("inferno")
    }

    fn sysname(&self) -> String {
        String::from("purg-probe")
    }

    fn current_pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    fn set_current_pid(&self, pid: u32) {
        self.pid.store(pid, Ordering::Relaxed);
    }
}
