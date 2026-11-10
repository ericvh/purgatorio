//! The EL1 virtual timer (`CNTV_*`) and the virtual counter.
//!
//! The timer's interrupt is a level-triggered PPI: INTID 27 on QEMU `virt`
//! ([`VIRT_TIMER_INTID`]), but take it from the DTB. It stays asserted while
//! the timer condition holds, so the IRQ handler must re-arm or
//! [`disable`] the timer before it EOIs the interrupt.
//!
//! If the kernel starts at EL2, the boot code should zero `CNTVOFF_EL2`
//! before dropping to EL1, so the virtual counter matches the physical one.

use core::arch::asm;

/// The virtual timer's PPI on QEMU `virt`.
pub const VIRT_TIMER_INTID: u32 = 27;

const CTL_ENABLE: u64 = 1 << 0;
const CTL_IMASK: u64 = 1 << 1;
const CTL_ISTATUS: u64 = 1 << 2;

const NS_PER_SEC: u128 = 1_000_000_000;

/// Counter frequency in Hz (`CNTFRQ_EL0`).
pub fn frequency() -> u64 {
    let v: u64;
    // SAFETY: reading CNTFRQ_EL0 has no side effects.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// The virtual counter (`CNTVCT_EL0`). The `isb` stops the read from being
/// performed early, ahead of earlier instructions.
pub fn counter() -> u64 {
    let v: u64;
    // SAFETY: reading CNTVCT_EL0 has no side effects.
    unsafe {
        asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags))
    };
    v
}

/// Converts counter ticks to nanoseconds, rounding down.
pub fn ticks_to_ns(t: u64) -> u64 {
    let hz = u128::from(frequency().max(1));
    u64::try_from(u128::from(t) * NS_PER_SEC / hz).unwrap_or(u64::MAX)
}

/// Converts nanoseconds to counter ticks, rounding up so a timer armed for
/// `ns` never fires early.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let hz = u128::from(frequency());
    u64::try_from((u128::from(ns) * hz).div_ceil(NS_PER_SEC)).unwrap_or(u64::MAX)
}

/// Arms the timer to fire when the counter reaches `cval`, with its
/// interrupt unmasked. Replaces any earlier deadline.
pub fn arm_at(cval: u64) {
    // SAFETY: CNTV_CVAL_EL0 and CNTV_CTL_EL0 belong to this module. The
    // isb makes the new deadline take effect (and an old assertion clear)
    // before the next instruction, such as a GIC EOI.
    unsafe {
        asm!(
            "msr cntv_cval_el0, {cval}",
            "msr cntv_ctl_el0, {ctl}",
            "isb",
            cval = in(reg) cval,
            ctl = in(reg) CTL_ENABLE,
            options(nostack, preserves_flags),
        )
    };
}

/// Arms the timer to fire `ticks` counter ticks from now.
pub fn arm_after(ticks: u64) {
    arm_at(counter().saturating_add(ticks));
}

/// Arms the timer to fire `ns` nanoseconds from now.
pub fn arm_after_ns(ns: u64) {
    arm_after(ns_to_ticks(ns));
}

/// Stops the timer; its interrupt deasserts.
pub fn disable() {
    // SAFETY: as in `arm_at`.
    unsafe {
        asm!(
            "msr cntv_ctl_el0, xzr",
            "isb",
            options(nostack, preserves_flags)
        )
    };
}

/// Whether the timer is enabled, unmasked and its deadline has passed:
/// that is, whether it is asserting its interrupt.
pub fn pending() -> bool {
    let ctl: u64;
    // SAFETY: reading CNTV_CTL_EL0 has no side effects.
    unsafe {
        asm!("isb", "mrs {}, cntv_ctl_el0", out(reg) ctl, options(nomem, nostack, preserves_flags))
    };
    ctl & (CTL_ENABLE | CTL_IMASK | CTL_ISTATUS) == CTL_ENABLE | CTL_ISTATUS
}
