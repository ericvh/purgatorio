//! The global allocator for an image: [`kalloc`]'s size classes and
//! per-CPU caches over the free RAM after the image, with every operation
//! run with IRQs masked on the CPU it started on.
//!
//! Interrupt handlers allocate and free too (preempting, the scheduler
//! grows its run queue and frees exited threads), so an interrupt must
//! never find a heap lock held on its own CPU, and no thread may be
//! preempted holding one. Masking also pins the thread to its CPU, which
//! is what lets each CPU use its own cache. Every critical section is
//! short and bounded (`kalloc`'s module docs), so masking costs little
//! interrupt latency.
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

use crate::cpu::{self, MAX_CPUS};
use crate::exception::IrqGuard;
use crate::kalloc::{self, Alloc};
use crate::mmu::{self, Region};

pub use crate::kalloc::REGIONS;

const _: () = assert!(MAX_CPUS <= kalloc::MAX_CPUS);

pub struct Heap(Alloc);

/// What the heap holds now, for `#c/memory`, `#%` and the kernel's
/// `memstat` boot argument.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes the heap was given.
    pub size: usize,
    /// Bytes allocated now, as requested (a size class rounds a small
    /// block up by at most a quarter, the large heaps to 16 bytes).
    pub used: usize,
    /// The most `used` has been, or has been since
    /// [`Heap::reset_peak`]. Sampled: it can miss up to
    /// [`kalloc::PEAK_STEP`] a CPU.
    pub peak: usize,
    /// The largest block that can be allocated now: less than
    /// `size - used` when the free space is fragmented.
    pub largest_free: usize,
    /// Blocks allocated now.
    pub live: usize,
    /// Allocations made since boot.
    pub allocs: u64,
    /// Bytes of free small blocks cached by the CPUs.
    pub cached: usize,
    /// Bytes of slabs the size classes hold, in use or not.
    pub slabs: usize,
}

impl Heap {
    pub const fn empty() -> Heap {
        Heap(Alloc::new())
    }

    /// Gives the heap the physical RAM `regions`, through the linear map.
    /// Empty regions, and any past the first [`REGIONS`], are ignored.
    ///
    /// # Safety
    /// Each region must be unused RAM, linear-mapped read-write, and this
    /// must be called once, on the boot CPU, before the first allocation
    /// and before another CPU starts.
    pub unsafe fn init(&self, regions: &[Region]) {
        // The boot CPU is CPU 0 from the first allocation on.
        cpu::set_id(0);
        let mut virt = [(core::ptr::null_mut(), 0); REGIONS];
        for (v, r) in virt.iter_mut().zip(regions.iter().filter(|r| r.size > 0)) {
            *v = (mmu::phys_to_virt(r.base) as *mut u8, r.size);
        }
        let _g = IrqGuard::new();
        // SAFETY: the caller guarantees the memory is ours.
        unsafe { self.0.regions().init(&virt) };
    }

    /// Adds the `by` bytes of RAM directly after the region that ends at
    /// physical `end` to it. Returns whether a region ends there.
    ///
    /// # Safety
    /// Those bytes must be unused RAM, linear-mapped read-write.
    pub unsafe fn extend(&self, end: usize, by: usize) -> bool {
        let _g = IrqGuard::new();
        // SAFETY: the caller guarantees the memory is ours.
        unsafe {
            self.0
                .regions()
                .extend(mmu::phys_to_virt(end) as *mut u8, by)
        }
    }

    /// The counters, and the largest free block, found by trying sizes
    /// (each try a short critical section of its own). For reports, not
    /// for hot paths.
    pub fn stats(&self) -> HeapStats {
        let s = {
            let _g = IrqGuard::new();
            self.0.stats()
        };
        let largest_free = self.0.regions().largest_free_with(IrqGuard::new);
        HeapStats {
            size: s.size,
            used: s.used,
            peak: s.peak,
            largest_free,
            live: s.live,
            allocs: s.allocs,
            cached: s.cached,
            slabs: s.slabs,
        }
    }

    /// Bytes in use and the heap's size, for `Platform::memory`.
    pub fn memory(&self) -> (u64, u64) {
        let _g = IrqGuard::new();
        (
            self.0.used_and_peak().0 as u64,
            self.0.regions().size() as u64,
        )
    }

    /// Bytes in use and their peak, without [`Heap::stats`]'s search.
    pub fn used_and_peak(&self) -> (usize, usize) {
        self.0.used_and_peak()
    }

    /// Restarts the peak from what is in use now, so a caller can measure
    /// the peak of one phase.
    pub fn reset_peak(&self) {
        self.0.reset_peak();
    }

    /// Gives back what the CPUs cache and the empty slabs, so that
    /// `largest_free` and the RAM in use are what callers hold. Takes each
    /// lock briefly, in turn, with IRQs masked for each step.
    pub fn reclaim(&self) {
        self.0.reclaim_with(IrqGuard::new);
    }
}

// SAFETY: `kalloc::Alloc` is a correct allocator when each CPU index is
// used by one caller at a time: IRQs are masked around every call, so the
// thread stays on the CPU whose index it passes and nothing else runs
// there until it returns.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _g = IrqGuard::new();
        // SAFETY: as above; a `GlobalAlloc` layout has a nonzero size.
        unsafe { self.0.alloc(cpu::id(), layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _g = IrqGuard::new();
        // SAFETY: as above, and the caller's contract.
        unsafe { self.0.dealloc(cpu::id(), ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        {
            let _g = IrqGuard::new();
            if self.0.resize_in_place(cpu::id(), layout, new_size) {
                return ptr;
            }
        }
        // Moving a block copies it with IRQs enabled: a large one can take
        // milliseconds.
        let Ok(new) = Layout::from_size_align(new_size, layout.align()) else {
            return core::ptr::null_mut();
        };
        // SAFETY: the caller's contract gives a nonzero `new_size`.
        let q = unsafe { self.alloc(new) };
        if !q.is_null() {
            // SAFETY: both blocks hold at least this many bytes, and are
            // distinct; `ptr` came from this heap with `layout`.
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, q, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        q
    }
}
