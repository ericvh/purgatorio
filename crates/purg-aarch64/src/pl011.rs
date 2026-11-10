//! PL011 UART. The base address comes from the DTB.

use core::ptr::{read_volatile, write_volatile};

const DR: usize = 0x00;
const FR: usize = 0x18;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;

/// A PL011 at a mapped MMIO address. QEMU's PL011 needs no set-up.
#[derive(Clone, Copy)]
pub struct Pl011 {
    base: usize,
}

impl Pl011 {
    /// # Safety
    /// `base` must be the address of a PL011, mapped as Device memory.
    pub const unsafe fn new(base: usize) -> Pl011 {
        Pl011 { base }
    }

    fn reg(&self, off: usize) -> *mut u32 {
        (self.base + off) as *mut u32
    }

    pub fn putc(&self, b: u8) {
        // SAFETY: `new` requires a mapped PL011 at `base`.
        unsafe {
            while read_volatile(self.reg(FR)) & FR_TXFF != 0 {
                core::hint::spin_loop();
            }
            write_volatile(self.reg(DR), u32::from(b));
        }
    }

    /// Writes `buf`, turning `\n` into `\r\n`.
    pub fn write(&self, buf: &[u8]) {
        for &b in buf {
            if b == b'\n' {
                self.putc(b'\r');
            }
            self.putc(b);
        }
    }

    /// Reads the bytes already received, without waiting.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let mut n = 0;
        while n < buf.len() {
            // SAFETY: as in `putc`.
            let b = unsafe {
                if read_volatile(self.reg(FR)) & FR_RXFE != 0 {
                    break;
                }
                read_volatile(self.reg(DR))
            };
            buf[n] = b as u8;
            n += 1;
        }
        n
    }
}

impl core::fmt::Write for Pl011 {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Pl011::write(self, s.as_bytes());
        Ok(())
    }
}
