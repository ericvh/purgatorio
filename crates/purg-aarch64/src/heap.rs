//! The global allocator for an image: a linked-list heap over RAM after the
//! image, with every operation run with IRQs masked.
//!
//! Interrupt handlers allocate and free too (preempting, the scheduler
//! grows its run queue and frees exited threads), so an interrupt must
//! never find the heap locked, and no thread may be preempted holding it.
//! Heap operations are short, so masking costs little interrupt latency.
//!
//! ```ignore
//! #[global_allocator]
//! static HEAP: purg_aarch64::heap::Heap = purg_aarch64::heap::Heap::empty();
//! ```

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use linked_list_allocator::LockedHeap;

use crate::exception::IrqGuard;
use crate::mmu::{self, Region};

/// The heap with its accounting. The counters change only with IRQs
/// masked, on one CPU, so plain loads and stores are enough.
pub struct Heap {
    inner: LockedHeap,
    size: AtomicUsize,
    used: AtomicUsize,
    peak: AtomicUsize,
    live: AtomicUsize,
    allocs: AtomicU64,
}

/// What the heap holds now, for `#c/memory`, `#%` and the kernel's
/// `memstat` boot argument.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes the heap was given.
    pub size: usize,
    /// Bytes allocated now, as requested (the allocator rounds each block
    /// up to 16 bytes on top of this).
    pub used: usize,
    /// The most `used` has been.
    pub peak: usize,
    /// The largest block that can be allocated now: less than
    /// `size - used` when the free space is fragmented.
    pub largest_free: usize,
    /// Blocks allocated now.
    pub live: usize,
    /// Allocations made since boot.
    pub allocs: u64,
}

impl Heap {
    pub const fn empty() -> Heap {
        Heap {
            inner: LockedHeap::empty(),
            size: AtomicUsize::new(0),
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            allocs: AtomicU64::new(0),
        }
    }

    /// Gives the heap the physical RAM `r`, through the linear map.
    ///
    /// # Safety
    /// `r` must be unused RAM, linear-mapped read-write, and this must be
    /// called once, before the first allocation.
    pub unsafe fn init(&self, r: Region) {
        let _g = IrqGuard::new();
        // SAFETY: the caller guarantees the memory is ours.
        unsafe {
            self.inner
                .lock()
                .init(mmu::phys_to_virt(r.base) as *mut u8, r.size)
        };
        self.size.store(r.size, Ordering::Relaxed);
    }

    /// The counters, and the largest free block, found by trying sizes:
    /// about thirty allocations that are freed at once, with IRQs masked
    /// throughout. For reports, not for hot paths.
    pub fn stats(&self) -> HeapStats {
        let _g = IrqGuard::new();
        let mut heap = self.inner.lock();
        let mut fits = |size: usize| {
            let Ok(layout) = Layout::from_size_align(size, 16) else {
                return false;
            };
            match heap.allocate_first_fit(layout) {
                Ok(p) => {
                    // SAFETY: `p` was just allocated with `layout` and is
                    // not used.
                    unsafe { heap.deallocate(p, layout) };
                    true
                }
                Err(()) => false,
            }
        };
        // Invariant: `lo` fits (or is 0) and `hi` does not.
        let (mut lo, mut hi) = (0, self.size.load(Ordering::Relaxed) + 1);
        while hi - lo > 16 {
            let mid = lo + (hi - lo) / 2;
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        HeapStats {
            size: self.size.load(Ordering::Relaxed),
            used: self.used.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            largest_free: lo,
            live: self.live.load(Ordering::Relaxed),
            allocs: self.allocs.load(Ordering::Relaxed),
        }
    }

    /// Bytes in use and the heap's size, for `Platform::memory`.
    pub fn memory(&self) -> (u64, u64) {
        let _g = IrqGuard::new();
        (
            self.used.load(Ordering::Relaxed) as u64,
            self.size.load(Ordering::Relaxed) as u64,
        )
    }
}

// SAFETY: forwards to `LockedHeap`'s `GlobalAlloc`, only adding IRQ
// masking around each call.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _g = IrqGuard::new();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { self.inner.alloc(layout) };
        if !p.is_null() {
            let used = self.used.load(Ordering::Relaxed) + layout.size();
            self.used.store(used, Ordering::Relaxed);
            if used > self.peak.load(Ordering::Relaxed) {
                self.peak.store(used, Ordering::Relaxed);
            }
            self.live
                .store(self.live.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
            self.allocs
                .store(self.allocs.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _g = IrqGuard::new();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { self.inner.dealloc(ptr, layout) };
        self.used.store(
            self.used.load(Ordering::Relaxed) - layout.size(),
            Ordering::Relaxed,
        );
        self.live
            .store(self.live.load(Ordering::Relaxed) - 1, Ordering::Relaxed);
    }
}
