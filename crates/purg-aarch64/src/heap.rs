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
//! The heap counts what it hands out (the bytes in use, their high-water
//! mark, and the calls), for `#c/memory` and the footprint reports. The
//! state changes only inside the IRQ-masked sections, on one CPU.
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

struct State {
    regions: [linked_list_allocator::Heap; REGIONS],
    /// The most bytes ever in use, or since [`Heap::reset_high_water`].
    high_water: usize,
    allocs: u64,
    frees: u64,
}

impl State {
    fn used(&self) -> usize {
        self.regions.iter().map(|r| r.used()).sum()
    }
}

pub struct Heap(Mutex<State>);

/// A snapshot of the heap's counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    /// Bytes in use, including the allocator's rounding.
    pub used: usize,
    /// Bytes the heap manages.
    pub size: usize,
    /// The most bytes in use at once.
    pub high_water: usize,
    /// Calls to `alloc` that succeeded.
    pub allocs: u64,
    /// Calls to `dealloc`.
    pub frees: u64,
}

impl Heap {
    pub const fn empty() -> Heap {
        Heap(Mutex::new(State {
            regions: [const { linked_list_allocator::Heap::empty() }; REGIONS],
            high_water: 0,
            allocs: 0,
            frees: 0,
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

    pub fn stats(&self) -> HeapStats {
        let _g = IrqGuard::new();
        let s = self.0.lock();
        HeapStats {
            used: s.used(),
            size: s.regions.iter().map(|r| r.size()).sum(),
            high_water: s.high_water,
            allocs: s.allocs,
            frees: s.frees,
        }
    }

    /// Restarts the high-water mark from what is in use now, so a caller
    /// can measure the peak of one phase.
    pub fn reset_high_water(&self) {
        let _g = IrqGuard::new();
        let mut s = self.0.lock();
        s.high_water = s.used();
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
        s.high_water = s.high_water.max(s.used());
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
            s.frees += 1;
        }
    }
}
