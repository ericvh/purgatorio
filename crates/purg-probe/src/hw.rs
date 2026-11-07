//! QEMU virt devices: PL011 UART, PL031 RTC, the generic timer, and Arm
//! semihosting for exit.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

const UART: usize = 0x0900_0000;
const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;

const RTC: usize = 0x0901_0000;
const RTC_DR: usize = 0x00;

fn reg(base: usize, off: usize) -> *mut u32 {
    (base + off) as *mut u32
}

pub fn uart_putc(b: u8) {
    // SAFETY: the PL011 is mapped as Device memory at this address on virt.
    unsafe {
        while read_volatile(reg(UART, UART_FR)) & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        write_volatile(reg(UART, UART_DR), u32::from(b));
    }
}

pub fn uart_write(buf: &[u8]) {
    for &b in buf {
        if b == b'\n' {
            uart_putc(b'\r');
        }
        uart_putc(b);
    }
}

/// Reads whatever bytes the UART has received, without waiting.
pub fn uart_read(buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        // SAFETY: as in `uart_putc`.
        let b = unsafe {
            if read_volatile(reg(UART, UART_FR)) & FR_RXFE != 0 {
                break;
            }
            read_volatile(reg(UART, UART_DR))
        };
        buf[n] = b as u8;
        n += 1;
    }
    n
}

/// Seconds since the Unix epoch from the PL031.
pub fn rtc_seconds() -> u64 {
    // SAFETY: the PL031 is mapped as Device memory at this address on virt.
    u64::from(unsafe { read_volatile(reg(RTC, RTC_DR)) })
}

pub fn counter() -> u64 {
    let v: u64;
    // SAFETY: reading the virtual counter has no side effects.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

pub fn counter_hz() -> u64 {
    let v: u64;
    // SAFETY: reading the counter frequency has no side effects.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Exits QEMU with `code` through semihosting `SYS_EXIT`
/// (`ADP_Stopped_ApplicationExit`). Needs QEMU's `-semihosting`.
pub fn exit(code: u32) -> ! {
    const SYS_EXIT: u64 = 0x18;
    const APPLICATION_EXIT: u64 = 0x20026;
    let block = [APPLICATION_EXIT, u64::from(code)];
    // SAFETY: `hlt #0xf000` is the AArch64 semihosting trap; the block
    // outlives the call.
    unsafe {
        asm!("hlt #0xf000", in("x0") SYS_EXIT, in("x1") block.as_ptr(), options(nostack));
    }
    loop {
        // SAFETY: parks the core if semihosting is disabled.
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}
