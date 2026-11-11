//! aarch64 bare-metal support for QEMU `virt`, shared by `purg-kernel` and
//! `purg-probe`: the boot path and the shared link script, the DTB, the
//! GIC, the timer and interrupt dispatch, exception vectors, the MMU, the
//! PL011 console, the PL031 clock, a preemptive scheduler, the heap, a
//! `purg_platform` `Platform`, virtio-mmio, a smoltcp network stack for
//! `#I`, a virtio-9p transport and a virtio-blk driver.
//!
//! Pure code (the [`dtb`] parser, the [`mmu`] table builder and the
//! [`board`] decisions) builds and is unit-tested on the host. Everything
//! that touches system registers or MMIO is compiled only for
//! `aarch64-unknown-none`.
#![cfg_attr(not(test), no_std)]

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
extern crate alloc;

pub mod board;
pub mod dtb;
pub mod mmu;

#[cfg(all(target_arch = "aarch64", target_os = "none", feature = "blk"))]
pub mod blk;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod boot;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod console;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod exception;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod exit;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod gic;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod heap;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod irq;
#[cfg(all(target_arch = "aarch64", target_os = "none", feature = "net"))]
pub mod net;
#[cfg(all(target_arch = "aarch64", target_os = "none", feature = "p9"))]
pub mod p9;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod pl011;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod pl031;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod platform;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod sched;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod semihost;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod timer;
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    any(feature = "net", feature = "p9", feature = "blk")
))]
pub mod virtio;
