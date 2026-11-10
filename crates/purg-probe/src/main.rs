//! `purg-probe`: a bare-metal aarch64 image for QEMU virt that runs the
//! current milestone's portable test subset on the same `purg-core` and
//! `purg-vm-wasmi` code the emu uses, with `#c` on the PL011, `#I` on
//! smoltcp over virtio-net, kprocs as preemptive kernel threads, and a
//! writable RAM root seeded with files embedded in the image.
//!
//! Everything below the shared core is `purg_aarch64`, as in `purg-kernel`:
//! the boot path and link script, the DTB, the GIC and timer, the
//! scheduler, the console, the platform and the network stack. The probe
//! adds only its script ([`probe`]).
//!
//! It prints each command's output on the serial console, then exits QEMU
//! with 0 if every command ended as expected and 1 otherwise: through
//! semihosting, or with `-append exit=psci` (KVM) through PSCI
//! `SYSTEM_OFF`, after a final `probe: pass` or `probe: N failed` line.
//! `cargo xtask probe [--kvm]` builds it, boots it and compares the serial
//! output with `expected.out`.
#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("purg-probe runs on aarch64-unknown-none only; use `cargo xtask probe`");
    std::process::exit(2);
}

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod probe;
