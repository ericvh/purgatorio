//! The global allocator for an image: linked-list heaps over the free RAM
//! after the image, with every operation run with IRQs masked.
//!
//! Interrupt handlers allocate and free too (preempting, the scheduler
//! grows its run queue and frees exited threads), so an interrupt must
//! never find the heap locked, and no thread may be preempted holding it.
//! Heap operations are short, so masking costs little interrupt latency.
//!
//! The free RAM is rarely one piece: the loader puts the DTB and an
//! initrd in the middle of it. The heap takes up to [`REGIONS`] stretches
//! and allocates from the first one with room, so a small machine does
//! not lose the RAM on the far side of the DTB.
//!
//! ```ignore
//! #[global_allocator]
//! static HEAP: purg_aarch64::heap::Heap = purg_aarch64::heap::Heap::empty();
//! ```

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;

use spin::Mutex;

use crate::exception::IrqGuard;
use crate::mmu::{self, Region};

/// The most stretches of RAM a heap is made of.
pub const REGIONS: usize = 4;

/// The heaps and their accounting. Everything changes only with IRQs
/// masked, on one CPU.
struct State {
    regions: [linked_list_allocator::Heap; REGIONS],
    used: usize,
    peak: usize,
    live: usize,
    allocs: u64,
}

pub struct Heap(Mutex<State>);

/// What the heap holds now, for `#c/memory`, `#%` and the kernel's
/// `memstat` boot argument.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes the heap was given.
    pub size: usize,
    /// Bytes allocated now, as requested (the allocator rounds each block
    /// up to 16 bytes on top of this).
    pub used: usize,
    /// The most `used` has been, or has been since
    /// [`Heap::reset_peak`].
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
        Heap(Mutex::new(State {
            regions: [const { linked_list_allocator::Heap::empty() }; REGIONS],
            used: 0,
            peak: 0,
            live: 0,
            allocs: 0,
        }))
    }

    /// Gives the heap the physical RAM `regions`, through the linear map.
    /// Empty regions, and any past the first [`REGIONS`], are ignored.
    ///
    /// # Safety
    /// Each region must be unused RAM, linear-mapped read-write, and this
    /// must be called once, before the first allocation.
    pub unsafe fn init(&self, regions: &[Region]) {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        let usable = regions.iter().filter(|r| r.size > 0);
        for (heap, r) in s.regions.iter_mut().zip(usable) {
            // SAFETY: the caller guarantees the memory is ours.
            unsafe { heap.init(mmu::phys_to_virt(r.base) as *mut u8, r.size) };
        }
    }

    /// Adds the `by` bytes of RAM directly after the region that ends at
    /// physical `end` to it. Returns whether a region ends there.
    ///
    /// # Safety
    /// Those bytes must be unused RAM, linear-mapped read-write.
    pub unsafe fn extend(&self, end: usize, by: usize) -> bool {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        let top = mmu::phys_to_virt(end) as *mut u8;
        match s
            .regions
            .iter_mut()
            .find(|r| r.size() > 0 && r.top() == top)
        {
            Some(r) => {
                // SAFETY: the caller guarantees the memory is ours.
                unsafe { r.extend(by) };
                true
            }
            None => false,
        }
    }

    /// The counters, and the largest free block, found by trying sizes:
    /// about thirty allocations per region that are freed at once, with
    /// IRQs masked throughout. For reports, not for hot paths.
    pub fn stats(&self) -> HeapStats {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        let mut largest_free = 0;
        for heap in s.regions.iter_mut().filter(|r| r.size() > 0) {
            let size = heap.size();
            let mut fits = |size: usize| {
                let Ok(layout) = Layout::from_size_align(size, 16) else {
                    return false;
                };
                match heap.allocate_first_fit(layout) {
                    Ok(p) => {
                        // SAFETY: `p` was just allocated with `layout` and
                        // is not used.
                        unsafe { heap.deallocate(p, layout) };
                        true
                    }
                    Err(()) => false,
                }
            };
            // Invariant: `lo` fits (or is 0) and `hi` does not.
            let (mut lo, mut hi) = (0, size + 1);
            while hi - lo > 16 {
                let mid = lo + (hi - lo) / 2;
                if fits(mid) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            largest_free = largest_free.max(lo);
        }
        HeapStats {
            size: s.regions.iter().map(|r| r.size()).sum(),
            used: s.used,
            peak: s.peak,
            largest_free,
            live: s.live,
            allocs: s.allocs,
        }
    }

    /// Bytes in use and the heap's size, for `Platform::memory`.
    pub fn memory(&self) -> (u64, u64) {
        let _g = IrqGuard::new();
        let s = self.0.lock();
        (
            s.used as u64,
            s.regions.iter().map(|r| r.size() as u64).sum(),
        )
    }

    /// Bytes in use and their peak, without [`Heap::stats`]'s search.
    pub fn used_and_peak(&self) -> (usize, usize) {
        let _g = IrqGuard::new();
        let s = self.0.lock();
        (s.used, s.peak)
    }

    /// Restarts the peak from what is in use now, so a caller can measure
    /// the peak of one phase.
    pub fn reset_peak(&self) {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        s.peak = s.used;
    }
}

// SAFETY: each block comes from one of the linked-list heaps and goes back
// to the one whose range holds it; IRQs are masked around every call.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        let Some(p) = s
            .regions
            .iter_mut()
            .find_map(|r| r.allocate_first_fit(layout).ok())
        else {
            return core::ptr::null_mut();
        };
        s.used += layout.size();
        s.peak = s.peak.max(s.used);
        s.live += 1;
        s.allocs += 1;
        p.as_ptr()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        let Some(p) = NonNull::new(ptr) else {
            return;
        };
        if let Some(r) = s
            .regions
            .iter_mut()
            .find(|r| r.bottom() <= ptr && ptr < r.top())
        {
            // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract,
            // and the block is in this region, so it came from it.
            unsafe { r.deallocate(p, layout) };
            s.used -= layout.size();
            s.live -= 1;
        }
    }
}
