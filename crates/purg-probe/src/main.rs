//! `purg-probe`: a bare-metal aarch64 image for QEMU virt that runs the
//! current milestone's portable test subset on the same `purg-core` and
//! `purg-vm-wasmi` code the emu uses, with `#c` on the PL011 and a
//! read-only root embedded in the image.
//!
//! It prints each command's output on the serial console, then exits QEMU
//! through semihosting with 0 if every command ended as expected and 1
//! otherwise. `cargo xtask probe` builds it, boots it and compares the
//! serial output with `expected.out`.
//!
//! Boot code ([`boot`]) and the hardware layer ([`hw`]) are the seed of the
//! Phase K kernel's.
#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("purg-probe runs on aarch64-unknown-none only; use `cargo xtask probe`");
    std::process::exit(2);
}

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod boot;
#[cfg(target_os = "none")]
mod hw;
#[cfg(target_os = "none")]
mod platform;
#[cfg(target_os = "none")]
mod probe;
