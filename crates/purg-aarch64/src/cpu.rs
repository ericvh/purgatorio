//! Per-CPU registers: which CPU this is, and which kernel thread it runs.
//!
//! - `TPIDRRO_EL0` holds the CPU's index (0 for the boot CPU), set once
//!   when the CPU comes up. The kernel has no EL0, so nothing else uses
//!   it. A thread may move to another CPU whenever it can be preempted, so
//!   [`id`] is only meaningful with IRQs masked.
//! - `TPIDR_EL1` holds a pointer to the running thread's scheduler entry,
//!   set by the scheduler on every switch. It follows the thread, not the
//!   CPU, so a thread can read it at any time, even with IRQs unmasked:
//!   if it is preempted and resumes on another CPU, that CPU's register
//!   points at the same thread.

use core::arch::asm;

/// The most CPUs the kernel brings up.
pub const MAX_CPUS: usize = 8;

/// This CPU's index. Call with IRQs masked, or the answer may be stale.
#[inline]
pub fn id() -> usize {
    let v: usize;
    // SAFETY: reading TPIDRRO_EL0 has no side effects.
    unsafe { asm!("mrs {}, tpidrro_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Records this CPU's index.
pub fn set_id(cpu: usize) {
    // SAFETY: TPIDRRO_EL0 is ours (see the module docs).
    unsafe { asm!("msr tpidrro_el0, {}", in(reg) cpu, options(nomem, nostack, preserves_flags)) };
}

/// The running thread's pointer, as set by [`set_thread`]; 0 before the
/// scheduler starts.
#[inline]
pub fn thread() -> usize {
    let v: usize;
    // SAFETY: reading TPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Records the running thread. Only the scheduler calls this, with IRQs
/// masked, as it switches.
pub fn set_thread(p: usize) {
    // SAFETY: TPIDR_EL1 is ours (see the module docs).
    unsafe { asm!("msr tpidr_el1, {}", in(reg) p, options(nomem, nostack, preserves_flags)) };
}

/// This CPU's MPIDR_EL1 affinity fields (Aff3..Aff0), as PSCI and the DTB
/// name CPUs.
pub fn mpidr() -> u64 {
    let v: u64;
    // SAFETY: reading MPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v & 0xff_00ff_ffff
}

/// Whether this CPU has the SHA-256 instructions (FEAT_SHA256):
/// `ID_AA64ISAR0_EL1.SHA2`, bits 15:12, is 1 (SHA-256) or 2 (SHA-512
/// too). The kernel assumes every CPU answers as the boot CPU does.
pub fn has_sha256() -> bool {
    let v: u64;
    // SAFETY: reading ID_AA64ISAR0_EL1 has no side effects.
    unsafe {
        asm!("mrs {}, id_aa64isar0_el1", out(reg) v, options(nomem, nostack, preserves_flags))
    };
    (v >> 12) & 0xf >= 1
}
