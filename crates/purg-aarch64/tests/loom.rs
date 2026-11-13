//! loom models of the kernel heap's per-CPU caches and depots
//! (`purg_aarch64::kalloc`). Run with
//! `RUSTFLAGS="--cfg loom" cargo test -p purg-aarch64 --test loom --release`.
//!
//! Each loom thread plays one CPU, as the bare-metal heap's callers do
//! with IRQs masked. The models check that no object is handed out twice,
//! that a block freed on another CPU than it came from goes home, that a
//! drain racing with a CPU's own frees loses nothing, and that the counts
//! come back to zero.
#![cfg(loom)]

use core::alloc::Layout;

use loom::sync::Arc;
use loom::thread;
use purg_aarch64::kalloc::{Alloc, cache_limit, class_of};

/// An allocator over `size` bytes of host memory, which lives as long as
/// it does.
struct Heap {
    a: Alloc,
    _mem: Vec<u128>,
}

impl Heap {
    fn new(size: usize) -> Arc<Heap> {
        let mut mem: Vec<u128> = vec![0; size / 16];
        let a = Alloc::new();
        // SAFETY: the memory is ours and outlives the allocator.
        unsafe { a.regions().init(&[(mem.as_mut_ptr().cast(), size)]) };
        Arc::new(Heap { a, _mem: mem })
    }

    fn alloc(&self, cpu: usize, l: Layout) -> usize {
        // SAFETY: each model thread is one CPU.
        let p = unsafe { self.a.alloc(cpu, l) };
        assert!(!p.is_null());
        // The block is ours alone: write a mark another owner would see.
        // SAFETY: `p` holds at least 8 bytes.
        unsafe { p.cast::<u64>().write(cpu as u64 + 1) };
        p as usize
    }

    fn free(&self, cpu: usize, p: usize, l: Layout, owner: usize) {
        // SAFETY: as above.
        assert_eq!(
            unsafe { (p as *const u64).read() },
            owner as u64 + 1,
            "a block had two owners"
        );
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { self.a.dealloc(cpu, p as *mut u8, l) };
    }
}

/// Large enough that the class's per-CPU cache overflows into the depot
/// after a few frees.
const L: Layout = match Layout::from_size_align(4096, 8) {
    Ok(l) => l,
    Err(_) => panic!(),
};

/// `loom::model`, run on a thread with a stack large enough to build an
/// `Alloc` on (the TLSF heap's lists are 3.5 KiB).
fn model(f: impl Fn() + Sync + Send + 'static) {
    let f = std::sync::Arc::new(f);
    loom::model(move || {
        let f = f.clone();
        thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || f())
            .unwrap()
            .join()
            .unwrap();
    });
}

#[test]
fn two_cpus_never_share_an_object() {
    model(|| {
        let h = Heap::new(256 << 10);
        let h2 = h.clone();
        let t = thread::spawn(move || {
            let a = h2.alloc(1, L);
            let b = h2.alloc(1, L);
            h2.free(1, a, L, 1);
            h2.free(1, b, L, 1);
        });
        let a = h.alloc(0, L);
        let b = h.alloc(0, L);
        h.free(0, b, L, 0);
        h.free(0, a, L, 0);
        t.join().unwrap();
        let s = h.a.stats();
        assert_eq!((s.used, s.live), (0, 0));
    });
}

#[test]
fn a_block_freed_on_another_cpu_goes_home() {
    let c = class_of(L).unwrap();
    // Enough frees on CPU 1 to overflow its cache into the depot.
    let n = cache_limit(c) + 1;
    model(move || {
        let h = Heap::new(256 << 10);
        let blocks: Vec<usize> = (0..n).map(|_| h.alloc(0, L)).collect();
        let h2 = h.clone();
        let t = thread::spawn(move || {
            for p in blocks {
                h2.free(1, p, L, 0);
            }
        });
        // CPU 0 keeps allocating while CPU 1 gives its blocks back.
        let p = h.alloc(0, L);
        h.free(0, p, L, 0);
        t.join().unwrap();
        h.a.reclaim();
        let s = h.a.stats();
        assert_eq!((s.used, s.live, s.cached, s.slabs), (0, 0, 0, 0), "{s:?}");
    });
}

#[test]
fn a_drain_races_a_cpus_frees() {
    model(|| {
        let h = Heap::new(256 << 10);
        let a = h.alloc(0, L);
        let b = h.alloc(0, L);
        let h2 = h.clone();
        // CPU 1 drains everything, as an allocation that ran out does.
        let t = thread::spawn(move || h2.a.reclaim());
        h.free(0, a, L, 0);
        let c = h.alloc(0, L);
        h.free(0, b, L, 0);
        h.free(0, c, L, 0);
        t.join().unwrap();
        h.a.reclaim();
        let s = h.a.stats();
        assert_eq!((s.used, s.live, s.cached, s.slabs), (0, 0, 0, 0), "{s:?}");
    });
}
