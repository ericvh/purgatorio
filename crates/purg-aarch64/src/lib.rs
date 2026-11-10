//! aarch64 bare-metal support for QEMU `virt`, shared by `purg-kernel` and
//! (later) `purg-probe`: the probe's boot stub and hardware layer are the
//! seed of this crate.
//!
//! Pure code (the [`dtb`] parser and the [`mmu`] table builder) builds and
//! is unit-tested on the host. Everything that touches system registers or
//! MMIO is compiled only for `aarch64-unknown-none`.
#![cfg_attr(not(test), no_std)]

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
extern crate alloc;

pub mod dtb;
pub mod mmu;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod exception;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod gic;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod pl011;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod sched;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod semihost;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod timer;
