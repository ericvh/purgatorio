//! Data cache geometry from `CTR_EL0`, for cache maintenance by address:
//! the virtio drivers' DMA buffers, and the boot path's invalidation of
//! what was written with the MMU off.
//!
//! QEMU's CPUs report 64-byte lines, but hardware need not (#28). The
//! smallest data cache line (`DminLine`) is the step of a `dc` loop, and
//! the cache writeback granule (`CWG`) is the most one written-back line
//! can cover, so DMA buffers are aligned and padded to it.

use core::arch::asm;

fn ctr() -> u64 {
    let ctr: u64;
    // SAFETY: reading CTR_EL0 has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    ctr
}

/// The smallest data cache line in bytes (`CTR_EL0.DminLine`): the step
/// for `dc cvac` and `dc ivac` loops.
pub fn dline() -> usize {
    4 << ((ctr() >> 16) & 0xf)
}

/// The alignment and padding for memory a device writes: the cache
/// writeback granule (`CTR_EL0.CWG`), or 128 bytes when the CPU does not
/// say (as Linux assumes), and never less than [`dline`] or 64.
pub fn dma_align() -> usize {
    let cwg = ((ctr() >> 24) & 0xf) as usize;
    let granule = if cwg == 0 { 128 } else { 4 << cwg };
    granule.max(dline()).max(64)
}

/// The smallest instruction cache line in bytes (`CTR_EL0.IminLine`):
/// the step for `ic ivau` loops.
pub fn iline() -> usize {
    4 << (ctr() & 0xf)
}
