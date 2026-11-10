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

use linked_list_allocator::LockedHeap;

use crate::exception::IrqGuard;
use crate::mmu::{self, Region};

pub struct Heap(LockedHeap);

impl Heap {
    pub const fn empty() -> Heap {
        Heap(LockedHeap::empty())
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
            self.0
                .lock()
                .init(mmu::phys_to_virt(r.base) as *mut u8, r.size)
        };
    }
}

// SAFETY: forwards to `LockedHeap`'s `GlobalAlloc`, only adding IRQ
// masking around each call.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _g = IrqGuard::new();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { self.0.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _g = IrqGuard::new();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { self.0.dealloc(ptr, layout) }
    }
}
