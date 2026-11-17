//! An allocator for threaded guests (`docs/threads.md`).
//!
//! A threaded guest's own allocator, wasi-libc's, has one lock: threads
//! that allocate run one after the other, and pay for the waits. This one
//! keeps small blocks in free lists that are sharded, so that threads
//! seldom meet on a lock, and goes to the system allocator only for a new
//! slab of blocks and for large ones. It is the global allocator of every
//! guest built for `wasm32-wasip1-threads` that links the SDK.
//!
//! - A block of up to [`SMALL`] bytes comes from a size class, a multiple
//!   of 16 bytes.
//! - There are [`SHARDS`] shards, each with a free list a class behind
//!   one spin lock held for a few instructions. Each thread is given a
//!   shard in turn the first time it allocates, so threads share one
//!   only when more than [`SHARDS`] have been started. The thread keeps
//!   its shard's number in a thread-local word that needs no setting up
//!   or tearing down, which an allocator could not do.
//! - A free goes to the freeing thread's shard, whoever allocated the
//!   block. A list that grows past [`HIGH`] blocks sends half of them to
//!   the class's central list, and an empty list takes a batch from there
//!   before it asks for a new slab; so memory one thread frees comes back
//!   to the threads that allocate.
//! - Slabs are never returned to the system allocator. The heap a guest
//!   holds is its high-water mark, as with most allocators of this kind.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::cell::UnsafeCell;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The largest block that comes from a size class.
pub const SMALL: usize = 1024;
/// The step between size classes, and their alignment.
const STEP: usize = 16;
const CLASSES: usize = SMALL / STEP;
/// The number of shards, a power of two.
pub const SHARDS: usize = 128;
/// A free list longer than this sends half its blocks to the central
/// list.
pub const HIGH: usize = 256;
/// The bytes of a new slab, about; at least [`MIN_BLOCKS`] blocks.
const SLAB: usize = 32 << 10;
const MIN_BLOCKS: usize = 8;

/// A free block: its first word links it to the next.
struct Block {
    next: Option<NonNull<Block>>,
}

/// A list of free blocks of one class.
#[derive(Clone, Copy)]
struct List {
    head: Option<NonNull<Block>>,
    len: usize,
}

impl List {
    const EMPTY: List = List { head: None, len: 0 };

    fn push(&mut self, b: NonNull<Block>) {
        // SAFETY: `b` is a free block of at least `STEP` bytes, aligned
        // for a pointer, that only this list refers to.
        unsafe { b.as_ptr().write(Block { next: self.head }) };
        self.head = Some(b);
        self.len += 1;
    }

    fn pop(&mut self) -> Option<NonNull<Block>> {
        let b = self.head?;
        // SAFETY: a block on the list was written by `push`.
        self.head = unsafe { b.as_ref().next };
        self.len -= 1;
        Some(b)
    }

    /// Takes the first `n` blocks off as a list of their own.
    fn split(&mut self, n: usize) -> List {
        let mut out = List::EMPTY;
        for _ in 0..n {
            match self.pop() {
                Some(b) => out.push(b),
                None => break,
            }
        }
        out
    }

    fn append(&mut self, mut other: List) {
        while let Some(b) = other.pop() {
            self.push(b);
        }
    }
}

/// A spin lock around a `T`. It is held for a few instructions, by code
/// that cannot block or allocate, so a waiter spins.
///
/// It is aligned to, and so padded to, 128 bytes, the largest cache line
/// in use (Apple's): two threads on neighbouring shards otherwise write
/// the same line, and two threads took nearly as long as one.
#[repr(align(128))]
struct Spin<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock gives one thread at a time the value, which holds
// pointers to free memory that belongs to whoever holds the lock.
unsafe impl<T> Sync for Spin<T> {}
// SAFETY: as above; the free memory is not tied to a thread.
unsafe impl<T> Send for Spin<T> {}

impl<T> Spin<T> {
    const fn new(value: T) -> Spin<T> {
        Spin {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the lock is held, so this is the only reference.
        let r = f(unsafe { &mut *self.value.get() });
        self.locked.store(false, Ordering::Release);
        r
    }
}

type Lists = [List; CLASSES];

/// The allocator. `const`, so that it can be a `static`.
pub struct Threaded {
    shards: [Spin<Lists>; SHARDS],
    central: [Spin<List>; CLASSES],
}

impl Default for Threaded {
    fn default() -> Threaded {
        Threaded::new()
    }
}

impl Threaded {
    pub const fn new() -> Threaded {
        Threaded {
            shards: [const { Spin::new([List::EMPTY; CLASSES]) }; SHARDS],
            central: [const { Spin::new(List::EMPTY) }; CLASSES],
        }
    }

    /// The class of a layout, or `None` for one the system allocator
    /// takes: a large block, or one aligned beyond a class's 16 bytes.
    fn class(layout: Layout) -> Option<usize> {
        (layout.size() <= SMALL && layout.align() <= STEP)
            .then(|| layout.size().max(1).div_ceil(STEP) - 1)
    }

    /// The calling thread's shard: the next in turn, chosen when the
    /// thread first allocates. (An earlier version picked one by a hash
    /// of the stack's address; on a 16-core machine two of 16 threads
    /// then often shared a shard and spun on its lock.)
    fn shard(&self) -> &Spin<Lists> {
        thread_local! {
            // The shard's number plus one; 0 before the first use. A
            // `const` initializer and no destructor: reading it neither
            // allocates nor registers anything.
            static MINE: Cell<usize> = const { Cell::new(0) };
        }
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let mine = MINE.with(|m| match m.get() {
            0 => {
                let n = NEXT.fetch_add(1, Ordering::Relaxed) % SHARDS + 1;
                m.set(n);
                n
            }
            n => n,
        });
        &self.shards[mine - 1]
    }

    /// Blocks for an empty list of `class`: a batch from the central
    /// list, or a new slab cut into blocks. `None` when the system
    /// allocator has no memory.
    fn refill(&self, class: usize) -> Option<List> {
        let batch = self.central[class].with(|c| c.split(HIGH / 2));
        if batch.len > 0 {
            return Some(batch);
        }
        let size = (class + 1) * STEP;
        let blocks = (SLAB / size).max(MIN_BLOCKS);
        let layout = Layout::from_size_align(blocks * size, STEP).ok()?;
        // SAFETY: the layout has a size that is not zero.
        let slab = NonNull::new(unsafe { System.alloc(layout) })?;
        let mut list = List::EMPTY;
        for i in (0..blocks).rev() {
            // SAFETY: block `i` lies inside the slab just allocated.
            list.push(unsafe { slab.add(i * size) }.cast());
        }
        Some(list)
    }
}

// SAFETY: a block handed out is on no list until it is freed, a class's
// blocks hold its largest layout at 16-byte alignment, and anything else
// goes to the system allocator both ways.
unsafe impl GlobalAlloc for Threaded {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = Threaded::class(layout) else {
            // SAFETY: the caller's.
            return unsafe { System.alloc(layout) };
        };
        let shard = self.shard();
        if let Some(b) = shard.with(|lists| lists[class].pop()) {
            return b.as_ptr().cast();
        }
        // Outside the shard's lock: a refill may take the central lock
        // or call the system allocator.
        let Some(mut fresh) = self.refill(class) else {
            return ptr::null_mut();
        };
        let b = fresh.pop();
        shard.with(|lists| lists[class].append(fresh));
        b.map_or(ptr::null_mut(), |b| b.as_ptr().cast())
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        let (Some(class), Some(b)) = (Threaded::class(layout), NonNull::new(at)) else {
            // SAFETY: the caller's.
            return unsafe { System.dealloc(at, layout) };
        };
        let spill = self.shard().with(|lists| {
            lists[class].push(b.cast());
            (lists[class].len > HIGH).then(|| lists[class].split(HIGH / 2))
        });
        if let Some(spill) = spill {
            self.central[class].with(|c| c.append(spill));
        }
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        match (Threaded::class(layout), Threaded::class(new)) {
            // Still fits its block.
            (Some(a), Some(b)) if a == b => at,
            // The system allocator's both ways: it may grow in place.
            // SAFETY: the caller's.
            (None, None) => unsafe { System.realloc(at, layout, new_size) },
            _ => {
                // SAFETY: the caller's; the copy is of what both hold.
                unsafe {
                    let to = self.alloc(new);
                    if !to.is_null() {
                        ptr::copy_nonoverlapping(at, to, layout.size().min(new_size));
                        self.dealloc(at, layout);
                    }
                    to
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    #[test]
    fn classes() {
        assert_eq!(Threaded::class(layout(0)), Some(0));
        assert_eq!(Threaded::class(layout(1)), Some(0));
        assert_eq!(Threaded::class(layout(16)), Some(0));
        assert_eq!(Threaded::class(layout(17)), Some(1));
        assert_eq!(Threaded::class(layout(SMALL)), Some(CLASSES - 1));
        assert_eq!(Threaded::class(layout(SMALL + 1)), None);
        assert_eq!(
            Threaded::class(Layout::from_size_align(64, 32).unwrap()),
            None
        );
    }

    #[test]
    fn blocks_are_distinct_and_reused() {
        let a = Threaded::new();
        let l = layout(40);
        // SAFETY: each block is written within its layout and freed once.
        unsafe {
            let blocks: Vec<*mut u8> = (0..1000).map(|_| a.alloc(l)).collect();
            for (i, b) in blocks.iter().enumerate() {
                assert!(!b.is_null() && b.addr() % 16 == 0);
                b.write_bytes(i as u8, 40);
            }
            for (i, b) in blocks.iter().enumerate() {
                assert_eq!(*b.add(39), i as u8);
            }
            let mut sorted = blocks.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), 1000);
            for b in &blocks {
                a.dealloc(*b, l);
            }
            // What was freed is handed out again.
            let again = a.alloc(l);
            assert!(blocks.contains(&again));
            a.dealloc(again, l);
        }
    }

    #[test]
    fn realloc_keeps_the_bytes() {
        let a = Threaded::new();
        // SAFETY: as above; each pointer is used with its current layout.
        unsafe {
            let p = a.alloc(layout(20));
            p.write_bytes(7, 20);
            // The same class: the same block.
            assert_eq!(a.realloc(p, layout(20), 30), p);
            // Another class, and then past the small ones and back.
            let q = a.realloc(p, layout(30), 200);
            assert_eq!(*q.add(19), 7);
            let r = a.realloc(q, layout(200), 5000);
            assert_eq!(*r.add(19), 7);
            let s = a.realloc(r, layout(5000), 8);
            assert_eq!(*s.add(7), 7);
            a.dealloc(s, layout(8));
        }
    }

    /// One thread allocates and another frees: what is freed comes back
    /// through the central list, and the heap does not grow without end.
    #[test]
    fn a_free_in_another_thread_comes_back() {
        static A: Threaded = Threaded::new();
        let l = layout(64);
        let (tx, rx) = std::sync::mpsc::sync_channel::<usize>(64);
        let freer = std::thread::spawn(move || {
            for at in rx {
                // SAFETY: the block was allocated with this layout and is
                // freed once, here.
                unsafe { A.dealloc(at as *mut u8, l) };
            }
        });
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200_000 {
            // SAFETY: the layout is not empty.
            let at = unsafe { A.alloc(l) } as usize;
            seen.insert(at);
            tx.send(at).unwrap();
        }
        drop(tx);
        freer.join().unwrap();
        // 200,000 allocations in far fewer blocks: the channel holds 64,
        // and a shard and the central list hold a few hundred each.
        assert!(seen.len() < 20_000, "{} blocks", seen.len());
    }

    #[test]
    fn many_threads() {
        let a = Arc::new(Threaded::new());
        let threads: Vec<_> = (0..8usize)
            .map(|t| {
                let a = a.clone();
                std::thread::spawn(move || {
                    let mut held: Vec<(*mut u8, Layout)> = Vec::new();
                    for i in 0..20_000usize {
                        let l = layout(1 + (i * 7 + t) % 1500);
                        // SAFETY: written within the layout, checked, and
                        // freed once with the same layout.
                        unsafe {
                            let p = a.alloc(l);
                            p.write_bytes(t as u8, l.size());
                            held.push((p, l));
                            if held.len() > 50 {
                                let (q, m) = held.swap_remove(i % held.len());
                                assert_eq!(*q, t as u8);
                                assert_eq!(*q.add(m.size() - 1), t as u8);
                                a.dealloc(q, m);
                            }
                        }
                    }
                    for (q, m) in held {
                        // SAFETY: as above.
                        unsafe { a.dealloc(q, m) };
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
}
