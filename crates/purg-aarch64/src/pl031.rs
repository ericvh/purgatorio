//! The PL031 real-time clock (QEMU `virt` has one): a 32-bit count of
//! seconds, read from the data register and set through the load register.
//! It backs `#r/rtc` (`purg_core::devrtc`).

use purg_platform::Rtc;

use crate::mmu;

/// Data register: the current count.
const RTCDR: usize = 0x00;
/// Load register: writing it sets the count.
const RTCLR: usize = 0x08;

pub struct Pl031 {
    base: usize,
}

impl Pl031 {
    /// # Safety
    /// `pa` must be a PL031 in the device window the high half maps.
    pub unsafe fn new(pa: usize) -> Pl031 {
        Pl031 {
            base: mmu::phys_to_virt(pa),
        }
    }
}

impl Rtc for Pl031 {
    fn seconds(&self) -> u64 {
        // SAFETY: `new`'s caller guarantees a mapped PL031.
        u64::from(unsafe { core::ptr::read_volatile((self.base + RTCDR) as *const u32) })
    }

    /// The count is 32 bits, so the PL031 holds times up to 2106.
    fn set_seconds(&self, secs: u64) -> purg_platform::Result<()> {
        let secs = u32::try_from(secs).map_err(|_| purg_abi::error::EBADARG)?;
        // SAFETY: `new`'s caller guarantees a mapped PL031.
        unsafe { core::ptr::write_volatile((self.base + RTCLR) as *mut u32, secs) };
        Ok(())
    }
}
