//! PL011 UART. The base address and interrupt come from the DTB.
//!
//! Receive can be polled ([`Pl011::read`]) or interrupt-driven: after
//! [`Pl011::enable_rx_interrupt`] the UART raises its interrupt while the
//! receive FIFO holds data (the receive interrupt at the FIFO trigger
//! level, the receive timeout interrupt for fewer bytes). The handler calls
//! [`Pl011::read`] until it returns less than it asked for, which empties
//! the FIFO and so lowers the level, then [`Pl011::clear_rx_interrupt`].

use core::ptr::{read_volatile, write_volatile};

const DR: usize = 0x00;
const FR: usize = 0x18;
const IFLS: usize = 0x34;
const IMSC: usize = 0x38;
const MIS: usize = 0x40;
const ICR: usize = 0x44;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
/// Receive interrupt.
const INT_RX: u32 = 1 << 4;
/// Receive timeout interrupt.
const INT_RT: u32 = 1 << 6;
/// IFLS RXIFLSEL field (bits 5:3); 0 triggers at 1/8 full.
const IFLS_RX_MASK: u32 = 0b111 << 3;

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

    /// Unmasks the receive and receive-timeout interrupts, with the receive
    /// trigger at its lowest level (1/8 of the FIFO). Clears any already
    /// raised; bytes already in the FIFO still raise the interrupt.
    pub fn enable_rx_interrupt(&self) {
        // SAFETY: as in `putc`.
        unsafe {
            let ifls = read_volatile(self.reg(IFLS));
            write_volatile(self.reg(IFLS), ifls & !IFLS_RX_MASK);
            write_volatile(self.reg(ICR), INT_RX | INT_RT);
            let imsc = read_volatile(self.reg(IMSC));
            write_volatile(self.reg(IMSC), imsc | INT_RX | INT_RT);
        }
    }

    /// Masks the receive interrupts.
    pub fn disable_rx_interrupt(&self) {
        // SAFETY: as in `putc`.
        unsafe {
            let imsc = read_volatile(self.reg(IMSC));
            write_volatile(self.reg(IMSC), imsc & !(INT_RX | INT_RT));
        }
    }

    /// Whether a receive interrupt is raised and unmasked.
    pub fn rx_interrupt_pending(&self) -> bool {
        // SAFETY: as in `putc`.
        unsafe { read_volatile(self.reg(MIS)) & (INT_RX | INT_RT) != 0 }
    }

    /// Clears the receive interrupts. Call after emptying the FIFO.
    pub fn clear_rx_interrupt(&self) {
        // SAFETY: as in `putc`.
        unsafe { write_volatile(self.reg(ICR), INT_RX | INT_RT) };
    }
}

impl core::fmt::Write for Pl011 {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Pl011::write(self, s.as_bytes());
        Ok(())
    }
}
