//! `purg-kernel`: the native aarch64 kernel for QEMU virt (Phase K).
//!
//! The boot path, the link script, the console, interrupts and the
//! scheduler are `purg_aarch64`'s, shared with `purg-probe`.
//!
//! K1: boot at EL2 or EL1 (`purg_aarch64::boot`), the MMU with an identity map and a
//! high half (`purg_aarch64::mmu`), a heap over the RAM after the image, a
//! PL011 console, the DTB, the GIC and a 100 Hz timer, and preemptive
//! kernel threads ([`irq`]). K2 adds locks that are never held across a
//! preemption, an idle CPU when every thread is blocked, and an
//! interrupt-driven console (`purg_aarch64::console`), with self-tests in
//! [`selftest`]. K3 adds `#r` and `#t` ([`devices`]). `purg_main`
//! prints a banner and self-tests, then exits QEMU: through semihosting
//! (exit code 0 or 1) or, with `-append exit=psci` (for KVM, where
//! semihosting does not work), through PSCI `SYSTEM_OFF` after a final
//! `kernel: ok` or `kernel: FAILED ...` line. `cargo xtask kernel` boots it
//! and compares the serial output with `expected.out`.
//!
//! With `memstat` in the bootargs the kernel also reports where its memory
//! goes ([`memstat`]); `docs/footprint.md` has the numbers.
//!
//! Memory map (QEMU virt; RAM size from the DTB):
//! - physical load address 0x4020_0000 (the DTB is at 0x4000_0000);
//! - the high half: virtual `KERNEL_VBASE + p` = physical `p`, with
//!   `KERNEL_VBASE` = 0xffff_0000_0000_0000, so the image runs at
//!   0xffff_0000_4020_0000;
//! - device window: physical 0..1 GiB, Device-nGnRnE, never executable;
//! - RAM from 0x4000_0000 linear-mapped Normal RW (the boot tables map a
//!   fixed 512 MiB window, the runtime tables what the DTB reports); the image's
//!   text RX, rodata R, data/bss RW, an unmapped guard page, then a 64 KiB
//!   boot stack RW;
//! - heap: the largest free stretch of RAM after the image;
//! - boot page tables: 16 pages in .bss (the map takes 8).
#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("purg-kernel runs on aarch64-unknown-none only; use `cargo xtask kernel`");
    std::process::exit(2);
}

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod devices;
#[cfg(target_os = "none")]
mod irq;
#[cfg(target_os = "none")]
mod kernel;
#[cfg(target_os = "none")]
mod memstat;
#[cfg(target_os = "none")]
mod selftest;
#[cfg(target_os = "none")]
mod system;
