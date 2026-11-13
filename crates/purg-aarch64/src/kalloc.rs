//! The kernel heap's allocator, without the hardware: size classes in
//! slabs, a cache per CPU in front of them, and a TLSF heap over the RAM
//! regions for slabs and large blocks. `heap` wraps it for the
//! bare-metal images (IRQs masked, the CPU from `TPIDRRO_EL0`); here it is
//! plain code, so it is unit-tested on the host and has loom models
//! (`tests/loom.rs`). `docs/perf.md` section 11 has the design and the
//! measurements.
//!
//! # Paths
//!
//! - A small block (at most [`SMALL_MAX`] bytes, aligned to at most
//!   [`SLAB_ALIGN`]) is rounded up to one of [`NCLASSES`] size classes,
//!   four to each doubling (at most a quarter wasted). Each CPU keeps a
//!   short free list per class; `alloc` and `dealloc` touch only that
//!   list, under the CPU's own lock, which only a drain from another CPU
//!   ever contends.
//! - When a CPU's list is empty it takes a batch from the class's depot,
//!   and when it is over its limit it gives a batch back. The depot is a
//!   list of slabs: blocks of 16 to 128 KiB cut into objects of the
//!   class. The class comes from the `Layout` that `dealloc` is given and
//!   the slab from a map of the heap (below), so objects carry no header.
//!   A slab whose objects are all free goes back to the large heap (one
//!   empty slab a class is kept).
//! - Larger blocks, and the slabs themselves, come from [`Regions`]: one
//!   TLSF heap (`rlsf`) over every stretch of RAM, whose allocation and
//!   free take constant time.
//!
//! Every critical section is short and bounded: a per-CPU list operation,
//! a batch of at most [`CHUNK`] objects in a depot, or one TLSF operation.
//! Slabs are released after the depot's lock is dropped, and no lock is
//! held across a walk of a list.
//!
//! # The slab map
//!
//! Slabs are not aligned to their size (TLSF would waste up to the
//! alignment on each). Instead each stretch of RAM starts with a map that
//! has one entry for each [`GRAN`] bytes: the slab, if any, holding the
//! last byte of that granule. An object's slab is the entry of its
//! granule if that slab starts at or before the object, and otherwise the
//! entry of the granule before: a slab is at least `GRAN` long, so the
//! slab of an object that ends inside a granule also holds the last byte
//! of the one before. A slab sets its entries (at most nine) when it is
//! made and clears them when it is released. The map costs four bytes a
//! granule, 0.025% of the heap.
//!
//! # Accounting
//!
//! `used`, `live` and `allocs` count what callers asked for, so blocks
//! cached in the per-CPU lists and the slabs are free, and a leak check
//! sees only what callers hold. Each CPU counts its own operations (a
//! block freed on another CPU makes that CPU's `used` go negative); the
//! sums are exact whenever no operation is in flight. The peak is sampled
//! whenever a CPU's `used` has grown by [`PEAK_STEP`] and on every visit
//! to a depot or the large heaps, so it can miss at most `PEAK_STEP` a
//! CPU.

use core::alloc::Layout;
use core::ptr::{self, NonNull};

use sync::{AtomicIsize, AtomicU64, AtomicUsize, Lock, Ordering};

/// The most CPUs an allocator serves; CPU indices are below this.
pub const MAX_CPUS: usize = 8;
/// The most stretches of RAM the heap is made of.
pub const REGIONS: usize = 4;
/// The most pieces of memory, regions and their extensions, the heap
/// maps.
const SEGS: usize = 2 * REGIONS;
/// The slab map's granule: the smallest slab.
pub const GRAN: usize = 16 << 10;
/// A map entry with no slab.
const NO_SLAB: u32 = u32::MAX;
/// The largest small block.
pub const SMALL_MAX: usize = 16 << 10;
/// The largest alignment a small block can have; objects start at a
/// multiple of this in their slab.
pub const SLAB_ALIGN: usize = 64;
/// Size classes.
pub const NCLASSES: usize = 36;
/// Objects moved to or from a depot under one hold of its lock.
pub const CHUNK: usize = 32;
/// Growth in a CPU's `used` that makes it sample the peak.
pub const PEAK_STEP: usize = 64 << 10;
/// Bytes a CPU caches of one class (at least [`MIN_CACHE`] objects).
const CACHE_BYTES: usize = 32 << 10;
const MIN_CACHE: usize = 4;
const MAX_CACHE: usize = 32;
/// Empty slabs a depot keeps instead of releasing.
const KEEP_EMPTY: usize = 1;
/// The slab header, at the start of each slab.
const HDR: usize = SLAB_ALIGN;

/// The locks and atomics: spin's under `no_std`, loom's in the models.
pub mod sync {
    #[cfg(not(loom))]
    pub use core::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};
    #[cfg(loom)]
    pub use loom::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};

    /// A fair (ticket) spin lock, so a contended depot serves CPUs in
    /// turn.
    pub struct Lock<T>(
        #[cfg(not(loom))] spin::mutex::TicketMutex<T>,
        #[cfg(loom)] loom::sync::Mutex<T>,
    );

    #[cfg(not(loom))]
    pub type Guard<'a, T> = spin::mutex::TicketMutexGuard<'a, T>;
    #[cfg(loom)]
    pub type Guard<'a, T> = loom::sync::MutexGuard<'a, T>;

    impl<T> Lock<T> {
        #[cfg(not(loom))]
        pub const fn new(v: T) -> Lock<T> {
            Lock(spin::mutex::TicketMutex::new(v))
        }
        #[cfg(loom)]
        pub fn new(v: T) -> Lock<T> {
            Lock(loom::sync::Mutex::new(v))
        }
        #[cfg(not(loom))]
        pub fn lock(&self) -> Guard<'_, T> {
            self.0.lock()
        }
        #[cfg(loom)]
        pub fn lock(&self) -> Guard<'_, T> {
            self.0.lock().unwrap()
        }
    }
}

const fn class_size(i: usize) -> usize {
    if i < 8 {
        (i + 1) * 16
    } else {
        let base = 128 << ((i - 8) / 4);
        base + ((i - 8) % 4 + 1) * (base / 4)
    }
}

/// The class sizes: 16 to 128 in steps of 16, then four to each doubling
/// up to [`SMALL_MAX`].
pub const CLASS_SIZES: [usize; NCLASSES] = {
    let mut t = [0; NCLASSES];
    let mut i = 0;
    while i < NCLASSES {
        t[i] = class_size(i);
        i += 1;
    }
    t
};

const _: () = assert!(CLASS_SIZES[NCLASSES - 1] == SMALL_MAX);

/// The smallest class that holds `size` bytes.
fn class_index(size: usize) -> usize {
    if size <= 128 {
        return size.max(1).div_ceil(16) - 1;
    }
    let shift = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize;
    let base = 1 << shift;
    8 + (shift - 7) * 4 + (size - 1 - base) / (base / 4)
}

/// The class of a block, or `None` for the large heaps. A class's
/// objects are aligned to the largest power of two dividing its size (up
/// to [`SLAB_ALIGN`]), so a stricter alignment moves up a class.
pub fn class_of(layout: Layout) -> Option<usize> {
    let (size, align) = (layout.size(), layout.align());
    if size > SMALL_MAX || align > SLAB_ALIGN {
        return None;
    }
    (class_index(size)..NCLASSES).find(|&c| CLASS_SIZES[c].is_multiple_of(align))
}

/// A class's slab size: a power of two, at least [`GRAN`], holding at
/// least seven objects.
pub const fn slab_size(c: usize) -> usize {
    let s = (CLASS_SIZES[c] * 8).next_power_of_two();
    if s < GRAN { GRAN } else { s }
}

fn slab_layout(c: usize) -> Layout {
    // SAFETY: the size is nonzero and the alignment a power of two.
    unsafe { Layout::from_size_align_unchecked(slab_size(c), SLAB_ALIGN) }
}

/// Objects a CPU caches of class `c` before it gives some back.
pub const fn cache_limit(c: usize) -> usize {
    let n = CACHE_BYTES / CLASS_SIZES[c];
    if n < MIN_CACHE {
        MIN_CACHE
    } else if n > MAX_CACHE {
        MAX_CACHE
    } else {
        n
    }
}

/// Objects moved between a CPU and a depot at once.
const fn batch(c: usize) -> usize {
    cache_limit(c) / 2
}

/// A free object: a link in a per-CPU list or a slab's free list.
struct Free {
    next: *mut Free,
}

/// A slab's header.
#[repr(C)]
struct Slab {
    /// The depot's partial or empty list.
    next: *mut Slab,
    prev: *mut Slab,
    /// Freed objects.
    free: *mut Free,
    /// Objects never handed out start at index `bump`.
    bump: u32,
    nobj: u32,
    inuse: u32,
    /// Whether the slab is on the partial list.
    partial: bool,
}

const _: () = assert!(core::mem::size_of::<Slab>() <= HDR);

impl Slab {
    fn full(&self) -> bool {
        self.free.is_null() && self.bump == self.nobj
    }
}

/// A class's slabs that have free objects: `partial` holds those with
/// some in use, `empty` (singly linked) those with none.
struct Depot {
    partial: *mut Slab,
    empty: *mut Slab,
    nempty: usize,
}

// SAFETY: the slabs are reached only under the depot's lock.
unsafe impl Send for Depot {}

impl Depot {
    const fn new() -> Depot {
        Depot {
            partial: ptr::null_mut(),
            empty: ptr::null_mut(),
            nempty: 0,
        }
    }

    unsafe fn link(&mut self, s: *mut Slab) {
        // SAFETY: `s` and the list's head are slabs of this depot.
        unsafe {
            (*s).prev = ptr::null_mut();
            (*s).next = self.partial;
            (*s).partial = true;
            if !self.partial.is_null() {
                (*self.partial).prev = s;
            }
        }
        self.partial = s;
    }

    unsafe fn unlink(&mut self, s: *mut Slab) {
        // SAFETY: `s` is on this depot's partial list.
        unsafe {
            let (p, n) = ((*s).prev, (*s).next);
            if p.is_null() {
                self.partial = n;
            } else {
                (*p).next = n;
            }
            if !n.is_null() {
                (*n).prev = p;
            }
            (*s).partial = false;
        }
    }

    /// An object of class `c`, or null if no slab has one.
    unsafe fn take(&mut self, c: usize) -> *mut Free {
        let mut s = self.partial;
        if s.is_null() {
            s = self.empty;
            if s.is_null() {
                return ptr::null_mut();
            }
            // SAFETY: `s` heads the empty list.
            unsafe {
                self.empty = (*s).next;
                self.nempty -= 1;
                self.link(s);
            }
        }
        // SAFETY: `s` is a slab of class `c` with a free object.
        unsafe {
            let o = if (*s).free.is_null() {
                let o = (s as usize + HDR + (*s).bump as usize * CLASS_SIZES[c]) as *mut Free;
                (*s).bump += 1;
                o
            } else {
                let o = (*s).free;
                (*s).free = (*o).next;
                o
            };
            (*s).inuse += 1;
            if (*s).full() {
                self.unlink(s);
            }
            o
        }
    }

    /// Returns object `o` to its slab `s`; returns the slab if it is now
    /// empty and should be released.
    unsafe fn put(&mut self, o: *mut Free, s: *mut Slab) -> Option<*mut Slab> {
        // SAFETY: `o` was handed out by `take` from slab `s`.
        unsafe {
            let was_full = (*s).full();
            (*o).next = (*s).free;
            (*s).free = o;
            (*s).inuse -= 1;
            if (*s).inuse == 0 {
                if (*s).partial {
                    self.unlink(s);
                }
                if self.nempty >= KEEP_EMPTY {
                    return Some(s);
                }
                (*s).next = self.empty;
                self.empty = s;
                self.nempty += 1;
            } else if was_full {
                self.link(s);
            }
        }
        None
    }

    /// Makes the fresh block `s` a slab of class `c`, on the partial list.
    unsafe fn add(&mut self, c: usize, s: *mut Slab) {
        // SAFETY: the caller gives a block of `slab_size(c)` bytes.
        unsafe {
            s.write(Slab {
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
                free: ptr::null_mut(),
                bump: 0,
                nobj: ((slab_size(c) - HDR) / CLASS_SIZES[c]) as u32,
                inuse: 0,
                partial: false,
            });
            self.link(s);
        }
    }
}

/// A CPU's free lists, one per class.
struct Cache {
    head: [*mut Free; NCLASSES],
    count: [u32; NCLASSES],
}

// SAFETY: the objects are reached only under the CPU's lock.
unsafe impl Send for Cache {}

impl Cache {
    const fn new() -> Cache {
        Cache {
            head: [ptr::null_mut(); NCLASSES],
            count: [0; NCLASSES],
        }
    }

    fn pop(&mut self, c: usize) -> *mut Free {
        let o = self.head[c];
        if !o.is_null() {
            // SAFETY: the objects on the list are free and ours.
            self.head[c] = unsafe { (*o).next };
            self.count[c] -= 1;
        }
        o
    }

    fn push(&mut self, c: usize, o: *mut Free) {
        // SAFETY: `o` is a free object of class `c`.
        unsafe { (*o).next = self.head[c] };
        self.head[c] = o;
        self.count[c] += 1;
    }

    /// Takes the first `n` objects of class `c` off as a chain.
    fn split(&mut self, c: usize, n: usize) -> *mut Free {
        let head = self.head[c];
        let mut last = head;
        for _ in 1..n {
            // SAFETY: the list has more than `n` objects.
            last = unsafe { (*last).next };
        }
        // SAFETY: as above.
        unsafe {
            self.head[c] = (*last).next;
            (*last).next = ptr::null_mut();
        }
        self.count[c] -= n as u32;
        head
    }
}

/// One CPU's cache and counters, on its own cache lines.
#[repr(align(128))]
struct Cpu {
    cache: Lock<Cache>,
    used: AtomicIsize,
    live: AtomicIsize,
    allocs: AtomicU64,
    /// `used` when this CPU last sampled the peak.
    mark: AtomicIsize,
}

impl Cpu {
    #[cfg(not(loom))]
    const fn new() -> Cpu {
        Cpu {
            cache: Lock::new(Cache::new()),
            used: AtomicIsize::new(0),
            live: AtomicIsize::new(0),
            allocs: AtomicU64::new(0),
            mark: AtomicIsize::new(0),
        }
    }
    #[cfg(loom)]
    fn new() -> Cpu {
        Cpu {
            cache: Lock::new(Cache::new()),
            used: AtomicIsize::new(0),
            live: AtomicIsize::new(0),
            allocs: AtomicU64::new(0),
            mark: AtomicIsize::new(0),
        }
    }
}

/// The TLSF heap: blocks up to 8 GiB, sixteen lists to each doubling.
type Tlsf = rlsf::Tlsf<'static, u32, u16, 28, 16>;

/// The TLSF heap and the stretches of RAM it was given.
struct Large {
    tlsf: Tlsf,
    /// Each region's start and end, as given.
    regions: [(usize, usize); REGIONS],
}

// SAFETY: the heap's memory is reached only under the `Regions` lock.
unsafe impl Send for Large {}

/// A piece of memory the heap maps: a region, or an extension of one. Set
/// once, before `Regions::nsegs` publishes it.
struct Seg {
    base: SegWord,
    end: SegWord,
    /// The slab map, at `base`.
    map: SegWord,
}

/// One TLSF heap over up to [`REGIONS`] stretches of memory, under one
/// lock, for the large blocks and the slabs; and the slab map.
///
/// The map's entries and the pieces are core atomics, not loom's: an
/// entry a caller depends on was written before a lock that orders it,
/// and the pieces before any allocation, so the models need not explore
/// them.
pub struct Regions {
    large: Lock<Large>,
    segs: [Seg; SEGS],
    nsegs: SegWord,
}

impl Seg {
    const fn new() -> Seg {
        Seg {
            base: SegWord::new(0),
            end: SegWord::new(0),
            map: SegWord::new(0),
        }
    }
}

type SegWord = core::sync::atomic::AtomicUsize;

impl Regions {
    #[cfg(not(loom))]
    pub const fn new() -> Regions {
        Regions {
            large: Lock::new(Large {
                tlsf: Tlsf::new(),
                regions: [(0, 0); REGIONS],
            }),
            segs: [const { Seg::new() }; SEGS],
            nsegs: SegWord::new(0),
        }
    }
    #[cfg(loom)]
    pub fn new() -> Regions {
        Regions {
            large: Lock::new(Large {
                tlsf: Tlsf::new(),
                regions: [(0, 0); REGIONS],
            }),
            segs: [const { Seg::new() }; SEGS],
            nsegs: SegWord::new(0),
        }
    }

    /// Adds the memory `regions` (start, length): empty ones, and any
    /// past the first [`REGIONS`], are ignored.
    ///
    /// # Safety
    /// Each region must be unused, writable memory, given to no other
    /// allocator, and this must be called once, before the first
    /// allocation.
    pub unsafe fn init(&self, regions: &[(*mut u8, usize)]) {
        let mut l = self.large.lock();
        let usable = regions.iter().filter(|r| r.1 > 0).take(REGIONS);
        for (i, &(base, size)) in usable.enumerate() {
            let base = base as usize;
            // SAFETY: the caller guarantees the memory is ours.
            if unsafe { self.add(&mut l, base, size) } {
                l.regions[i] = (base, base + size);
            }
        }
    }

    /// Adds the `by` bytes after the region that ends at `top` to it.
    /// Returns whether a region ends there (and there was room to map
    /// the new piece).
    ///
    /// # Safety
    /// Those bytes must be unused, writable memory.
    pub unsafe fn extend(&self, top: *mut u8, by: usize) -> bool {
        let mut l = self.large.lock();
        let top = top as usize;
        let Some(i) = l.regions.iter().position(|r| r.1 > r.0 && r.1 == top) else {
            return false;
        };
        // SAFETY: the caller guarantees the memory is ours.
        if !unsafe { self.add(&mut l, top, by) } {
            return false;
        }
        l.regions[i].1 += by;
        true
    }

    /// Maps the piece `base..base + size` and gives the rest of it to the
    /// TLSF heap. Returns false if it is too small or there is no room.
    ///
    /// # Safety
    /// As [`Regions::init`].
    unsafe fn add(&self, l: &mut Large, base: usize, size: usize) -> bool {
        let n = self.nsegs.load(core::sync::atomic::Ordering::Relaxed);
        let map_bytes = (size.div_ceil(GRAN) * 4).next_multiple_of(SLAB_ALIGN);
        if n == SEGS || !base.is_multiple_of(16) || size < map_bytes + GRAN {
            return false;
        }
        // SAFETY: the memory is ours; the map is at its start.
        unsafe { core::slice::from_raw_parts_mut(base as *mut u32, map_bytes / 4).fill(NO_SLAB) };
        let seg = &self.segs[n];
        let relaxed = core::sync::atomic::Ordering::Relaxed;
        seg.base.store(base, relaxed);
        seg.end.store(base + size, relaxed);
        seg.map.store(base, relaxed);
        self.nsegs
            .store(n + 1, core::sync::atomic::Ordering::Release);
        let rest = ptr::slice_from_raw_parts_mut((base + map_bytes) as *mut u8, size - map_bytes);
        // SAFETY: the caller guarantees the memory is ours.
        let _ = unsafe { l.tlsf.insert_free_block_ptr(NonNull::new_unchecked(rest)) };
        true
    }

    /// The piece holding address `a`, as (base, end, map).
    fn seg(&self, a: usize) -> (usize, usize, *mut u32) {
        let n = self.nsegs.load(core::sync::atomic::Ordering::Acquire);
        let relaxed = core::sync::atomic::Ordering::Relaxed;
        for seg in &self.segs[..n] {
            let (base, end) = (seg.base.load(relaxed), seg.end.load(relaxed));
            if base <= a && a < end {
                return (base, end, seg.map.load(relaxed) as *mut u32);
            }
        }
        unreachable!("kalloc: {a:#x} is not in the heap");
    }

    fn entry(map: *mut u32, g: usize) -> &'static core::sync::atomic::AtomicU32 {
        // SAFETY: `g` is a granule of the piece whose map this is; the map
        // lives as long as the heap, and is only reached atomically.
        unsafe { core::sync::atomic::AtomicU32::from_ptr(map.add(g)) }
    }

    /// Sets (or, when `on` is false, clears) the map entries of the slab
    /// at `s`, `len` bytes long: those of the granules whose last byte it
    /// holds.
    fn map_slab(&self, s: usize, len: usize, on: bool) {
        let (base, end, map) = self.seg(s);
        let off = ((s - base) / 16) as u32;
        for g in (s - base) / GRAN..=(s + len - 1 - base) / GRAN {
            let last = (base + (g + 1) * GRAN).min(end) - 1;
            if s <= last && last < s + len {
                let e = Self::entry(map, g);
                if on {
                    e.store(off, core::sync::atomic::Ordering::Relaxed);
                } else if e.load(core::sync::atomic::Ordering::Relaxed) == off {
                    e.store(NO_SLAB, core::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// The slab holding the object at `o`.
    fn slab_of(&self, o: usize) -> usize {
        let (base, _, map) = self.seg(o);
        let g = (o - base) / GRAN;
        let at = |g: usize| Self::entry(map, g).load(core::sync::atomic::Ordering::Relaxed);
        let e = at(g);
        if e != NO_SLAB && base + e as usize * 16 <= o {
            return base + e as usize * 16;
        }
        let e = at(g - 1);
        debug_assert_ne!(e, NO_SLAB);
        base + e as usize * 16
    }

    /// The bytes the regions hold.
    pub fn size(&self) -> usize {
        self.large.lock().regions.iter().map(|r| r.1 - r.0).sum()
    }

    /// The largest block that can be allocated now, found by trying
    /// sizes. Each try takes the lock on its own, so the answer can be
    /// stale, and no hold is longer than two TLSF operations.
    pub fn largest_free(&self) -> usize {
        self.largest_free_with(|| ())
    }

    /// [`Regions::largest_free`], holding what `enter` returns around each
    /// try (the bare-metal heap masks IRQs).
    pub fn largest_free_with<G>(&self, enter: impl Fn() -> G) -> usize {
        let size = {
            let _g = enter();
            self.size()
        };
        let fits = |size: usize| {
            let Ok(layout) = Layout::from_size_align(size, 16) else {
                return false;
            };
            let _g = enter();
            let mut l = self.large.lock();
            match l.tlsf.allocate(layout) {
                Some(p) => {
                    // SAFETY: `p` was just allocated with `layout`.
                    unsafe { l.tlsf.deallocate(p, layout.align()) };
                    true
                }
                None => false,
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
        lo
    }

    fn alloc(&self, layout: Layout) -> *mut u8 {
        self.large
            .lock()
            .tlsf
            .allocate(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    /// # Safety
    /// `p` came from [`Regions::alloc`] with `layout`.
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let Some(nn) = NonNull::new(p) else {
            return;
        };
        // SAFETY: the caller's contract.
        unsafe { self.large.lock().tlsf.deallocate(nn, layout.align()) };
    }
}

impl Default for Regions {
    fn default() -> Regions {
        Regions::new()
    }
}

/// What the allocator holds now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes the regions hold.
    pub size: usize,
    /// Bytes allocated now, as requested.
    pub used: usize,
    /// The most `used` has been (sampled; see the module docs).
    pub peak: usize,
    /// Blocks allocated now.
    pub live: usize,
    /// Allocations made since the start.
    pub allocs: u64,
    /// Bytes of free objects in the per-CPU caches.
    pub cached: usize,
    /// Bytes of slabs taken from the regions.
    pub slabs: usize,
}

/// The allocator: per-CPU caches, depots and the regions.
pub struct Alloc {
    cpus: [Cpu; MAX_CPUS],
    depots: [Lock<Depot>; NCLASSES],
    regions: Regions,
    peak: AtomicUsize,
    slab_bytes: AtomicUsize,
}

impl Alloc {
    #[cfg(not(loom))]
    pub const fn new() -> Alloc {
        Alloc {
            cpus: [const { Cpu::new() }; MAX_CPUS],
            depots: [const { Lock::new(Depot::new()) }; NCLASSES],
            regions: Regions::new(),
            peak: AtomicUsize::new(0),
            slab_bytes: AtomicUsize::new(0),
        }
    }
    #[cfg(loom)]
    pub fn new() -> Alloc {
        Alloc {
            cpus: core::array::from_fn(|_| Cpu::new()),
            depots: core::array::from_fn(|_| Lock::new(Depot::new())),
            regions: Regions::new(),
            peak: AtomicUsize::new(0),
            slab_bytes: AtomicUsize::new(0),
        }
    }

    /// The large heaps, to give them memory.
    pub fn regions(&self) -> &Regions {
        &self.regions
    }

    /// Allocates `layout` for CPU `cpu`. Returns null when nothing is
    /// left, after draining every CPU's cache.
    ///
    /// # Safety
    /// `layout` has a nonzero size, `cpu` is below [`MAX_CPUS`], and no
    /// other call with the same `cpu` runs until this one returns.
    pub unsafe fn alloc(&self, cpu: usize, layout: Layout) -> *mut u8 {
        let c = class_of(layout);
        let mut p = self.get(cpu, c, layout);
        if p.is_null() {
            self.reclaim();
            p = self.get(cpu, c, layout);
            if p.is_null() {
                return p;
            }
        }
        self.account(cpu, layout.size() as isize, 1);
        p
    }

    /// Frees `p`.
    ///
    /// # Safety
    /// `p` came from this allocator with `layout`, and `cpu` is as for
    /// [`Alloc::alloc`].
    pub unsafe fn dealloc(&self, cpu: usize, p: *mut u8, layout: Layout) {
        if p.is_null() {
            return;
        }
        match class_of(layout) {
            // SAFETY: the caller's contract.
            Some(c) => unsafe { self.put(cpu, c, p) },
            // SAFETY: as above; large blocks come from the regions.
            None => unsafe { self.regions.dealloc(p, layout) },
        }
        self.account(cpu, -(layout.size() as isize), -1);
    }

    /// Accounts for resizing a block of `layout` to `new_size` where it
    /// is, if the new size is in the same class. Returns whether it can.
    pub fn resize_in_place(&self, cpu: usize, layout: Layout, new_size: usize) -> bool {
        let Ok(new) = Layout::from_size_align(new_size, layout.align()) else {
            return false;
        };
        let c = class_of(layout);
        if c.is_none() || class_of(new) != c {
            return false;
        }
        let me = &self.cpus[cpu];
        let u = me.used.load(Ordering::Relaxed) + new_size as isize - layout.size() as isize;
        me.used.store(u, Ordering::Relaxed);
        self.mark(cpu, u);
        true
    }

    /// Resizes `p` to `new_size`, in place if it stays in its class.
    ///
    /// # Safety
    /// As `GlobalAlloc::realloc`, and `cpu` as for [`Alloc::alloc`].
    pub unsafe fn realloc(
        &self,
        cpu: usize,
        p: *mut u8,
        layout: Layout,
        new_size: usize,
    ) -> *mut u8 {
        if self.resize_in_place(cpu, layout, new_size) {
            return p;
        }
        let Ok(new) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        // SAFETY: the caller's contract.
        let q = unsafe { self.alloc(cpu, new) };
        if !q.is_null() {
            // SAFETY: both blocks are at least this long, and distinct.
            unsafe {
                ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
                self.dealloc(cpu, p, layout);
            }
        }
        q
    }

    fn account(&self, cpu: usize, bytes: isize, blocks: isize) {
        let me = &self.cpus[cpu];
        let u = me.used.load(Ordering::Relaxed) + bytes;
        me.used.store(u, Ordering::Relaxed);
        me.live
            .store(me.live.load(Ordering::Relaxed) + blocks, Ordering::Relaxed);
        if blocks > 0 {
            me.allocs
                .store(me.allocs.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        self.mark(cpu, u);
    }

    /// Samples the peak when CPU `cpu`'s `used`, now `u`, has grown by
    /// [`PEAK_STEP`] since it last did.
    fn mark(&self, cpu: usize, u: isize) {
        let me = &self.cpus[cpu];
        let m = me.mark.load(Ordering::Relaxed);
        if u < m {
            me.mark.store(u, Ordering::Relaxed);
        } else if u - m >= PEAK_STEP as isize {
            me.mark.store(u, Ordering::Relaxed);
            self.sample_peak();
        }
    }

    fn total(&self, f: impl Fn(&Cpu) -> isize) -> usize {
        self.cpus.iter().map(f).sum::<isize>().max(0) as usize
    }

    fn used(&self) -> usize {
        self.total(|c| c.used.load(Ordering::Relaxed))
    }

    fn sample_peak(&self) {
        self.peak.fetch_max(self.used(), Ordering::Relaxed);
    }

    /// A block for `layout` from CPU `cpu`'s cache, a depot or the
    /// regions; null if there is none.
    fn get(&self, cpu: usize, c: Option<usize>, layout: Layout) -> *mut u8 {
        let Some(c) = c else {
            let p = self.regions.alloc(layout);
            self.sample_peak();
            return p;
        };
        let me = &self.cpus[cpu];
        let o = me.cache.lock().pop(c);
        if !o.is_null() {
            return o.cast();
        }
        let (o, rest, n) = self.refill(c);
        if n > 1 {
            let mut cache = me.cache.lock();
            let mut r = rest;
            while !r.is_null() {
                // SAFETY: `r` is on the chain `refill` made.
                let next = unsafe { (*r).next };
                cache.push(c, r);
                r = next;
            }
        }
        self.sample_peak();
        o.cast()
    }

    /// Up to a batch of class `c`'s objects from its depot, making a new
    /// slab if it has none: the first, the chain of the rest, and how
    /// many there are in all.
    fn refill(&self, c: usize) -> (*mut Free, *mut Free, usize) {
        let want = batch(c).max(1);
        let mut fresh: *mut Slab = ptr::null_mut();
        loop {
            let mut d = self.depots[c].lock();
            if !fresh.is_null() {
                // SAFETY: `fresh` is a new block of the slab's size.
                unsafe { d.add(c, fresh) };
            }
            let (mut first, mut rest, mut n) = (ptr::null_mut(), ptr::null_mut(), 0);
            while n < want {
                // SAFETY: the depot's slabs are of class `c`.
                let o = unsafe { d.take(c) };
                if o.is_null() {
                    break;
                }
                if n == 0 {
                    first = o;
                } else {
                    // SAFETY: `o` is ours now.
                    unsafe { (*o).next = rest };
                    rest = o;
                }
                n += 1;
            }
            drop(d);
            if n > 0 || !fresh.is_null() {
                return (first, rest, n);
            }
            fresh = self.regions.alloc(slab_layout(c)).cast();
            if fresh.is_null() {
                return (ptr::null_mut(), ptr::null_mut(), 0);
            }
            self.regions.map_slab(fresh as usize, slab_size(c), true);
            self.slab_bytes.fetch_add(slab_size(c), Ordering::Relaxed);
        }
    }

    unsafe fn put(&self, cpu: usize, c: usize, p: *mut u8) {
        let mut cache = self.cpus[cpu].cache.lock();
        cache.push(c, p.cast());
        if cache.count[c] as usize <= cache_limit(c) {
            return;
        }
        let chain = cache.split(c, batch(c));
        drop(cache);
        // SAFETY: the chain's objects are free objects of class `c`.
        unsafe { self.flush(c, chain) };
    }

    /// Gives the chain of free objects of class `c` back to their slabs,
    /// [`CHUNK`] at a time, releasing slabs that empty.
    unsafe fn flush(&self, c: usize, mut o: *mut Free) {
        while !o.is_null() {
            let mut release = [ptr::null_mut::<Slab>(); CHUNK];
            let mut nr = 0;
            let mut d = self.depots[c].lock();
            for _ in 0..CHUNK {
                if o.is_null() {
                    break;
                }
                // SAFETY: `o` is a free object of class `c`.
                let next = unsafe { (*o).next };
                let s = self.regions.slab_of(o as usize) as *mut Slab;
                // SAFETY: as above, from slab `s`.
                if let Some(s) = unsafe { d.put(o, s) } {
                    release[nr] = s;
                    nr += 1;
                }
                o = next;
            }
            drop(d);
            for &s in &release[..nr] {
                // SAFETY: the slab came from the regions with this layout
                // and none of its objects is in use.
                unsafe { self.release(c, s) };
            }
        }
    }

    /// Gives the empty slab `s` of class `c` back to the regions.
    ///
    /// # Safety
    /// No object of `s` is in use, and no depot holds it.
    unsafe fn release(&self, c: usize, s: *mut Slab) {
        self.regions.map_slab(s as usize, slab_size(c), false);
        // SAFETY: the caller's contract; slabs come from the regions.
        unsafe { self.regions.dealloc(s.cast(), slab_layout(c)) };
        self.slab_bytes.fetch_sub(slab_size(c), Ordering::Relaxed);
    }

    /// Empties CPU `cpu`'s cache into the depots.
    fn drain<G>(&self, cpu: usize, enter: &impl Fn() -> G) {
        for c in 0..NCLASSES {
            let _g = enter();
            let chain = {
                let mut cache = self.cpus[cpu].cache.lock();
                let n = cache.count[c] as usize;
                if n == 0 {
                    continue;
                }
                cache.split(c, n)
            };
            // SAFETY: the chain's objects are free objects of class `c`.
            unsafe { self.flush(c, chain) };
        }
    }

    /// Releases the empty slabs every depot keeps.
    fn trim_depots<G>(&self, enter: &impl Fn() -> G) {
        for c in 0..NCLASSES {
            loop {
                let _g = enter();
                let s = {
                    let mut d = self.depots[c].lock();
                    let s = d.empty;
                    if s.is_null() {
                        break;
                    }
                    // SAFETY: `s` heads the empty list.
                    d.empty = unsafe { (*s).next };
                    d.nempty -= 1;
                    s
                };
                // SAFETY: an empty slab has no object in use.
                unsafe { self.release(c, s) };
            }
        }
    }

    /// Gives everything cached back: every CPU's cache, and the empty
    /// slabs. For an allocation that found no room, and before reports.
    pub fn reclaim(&self) {
        self.reclaim_with(|| ());
    }

    /// [`Alloc::reclaim`], holding what `enter` returns around each step
    /// (one class of one CPU, or one slab): the bare-metal heap masks IRQs
    /// for each step rather than for the whole.
    pub fn reclaim_with<G>(&self, enter: impl Fn() -> G) {
        for cpu in 0..MAX_CPUS {
            self.drain(cpu, &enter);
        }
        self.trim_depots(&enter);
    }

    /// Bytes in use and their peak.
    pub fn used_and_peak(&self) -> (usize, usize) {
        let u = self.used();
        (u, self.peak.load(Ordering::Relaxed).max(u))
    }

    /// Restarts the peak from what is in use now.
    pub fn reset_peak(&self) {
        self.peak.store(self.used(), Ordering::Relaxed);
    }

    /// The counters. Takes each CPU's lock once, briefly.
    pub fn stats(&self) -> Stats {
        let (used, peak) = self.used_and_peak();
        let cached = self
            .cpus
            .iter()
            .map(|cpu| {
                let cache = cpu.cache.lock();
                (0..NCLASSES)
                    .map(|c| cache.count[c] as usize * CLASS_SIZES[c])
                    .sum::<usize>()
            })
            .sum();
        Stats {
            size: self.regions.size(),
            used,
            peak,
            live: self.total(|c| c.live.load(Ordering::Relaxed)),
            allocs: self
                .cpus
                .iter()
                .map(|c| c.allocs.load(Ordering::Relaxed))
                .sum(),
            cached,
            slabs: self.slab_bytes.load(Ordering::Relaxed),
        }
    }
}

impl Default for Alloc {
    fn default() -> Alloc {
        Alloc::new()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// An allocator over `mib` MiB of host memory.
    fn with_heap(mib: usize) -> (Alloc, Vec<u128>) {
        let mut mem: Vec<u128> = vec![0; (mib << 20) / 16];
        let a = Alloc::new();
        // SAFETY: the memory is ours and outlives the allocator's use.
        unsafe {
            a.regions()
                .init(&[(mem.as_mut_ptr().cast(), mem.len() * 16)])
        };
        (a, mem)
    }

    #[test]
    fn classes() {
        for size in 1..=SMALL_MAX {
            let c = class_index(size);
            assert!(CLASS_SIZES[c] >= size, "{size}");
            assert!(c == 0 || CLASS_SIZES[c - 1] < size, "{size}");
            assert!(
                CLASS_SIZES[c] * 4 <= size * 5 + 64,
                "{size} wastes too much"
            );
        }
        for (c, &size) in CLASS_SIZES.iter().enumerate() {
            assert!(slab_size(c).is_power_of_two());
            assert!((slab_size(c) - HDR) / size >= 7, "class {c}");
        }
        let l = |s, a| Layout::from_size_align(s, a).unwrap();
        assert_eq!(class_of(l(48, 32)).map(|c| CLASS_SIZES[c]), Some(64));
        assert_eq!(class_of(l(16, 64)).map(|c| CLASS_SIZES[c]), Some(64));
        assert_eq!(class_of(l(SMALL_MAX + 1, 8)), None);
        assert_eq!(class_of(l(64, 128)), None);
    }

    #[test]
    fn alignment_and_contents() {
        let (a, _mem) = with_heap(8);
        let mut held = Vec::new();
        for (i, &(size, align)) in [
            (1, 1),
            (24, 8),
            (48, 32),
            (100, 64),
            (3000, 16),
            (20000, 4096),
            (16384, 64),
            (70, 128),
        ]
        .iter()
        .cycle()
        .take(400)
        .enumerate()
        {
            let layout = Layout::from_size_align(size, align).unwrap();
            // SAFETY: CPU 0 is used by this thread alone.
            let p = unsafe { a.alloc(0, layout) };
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "{size} aligned {align}");
            // SAFETY: `p` holds `size` bytes.
            unsafe { ptr::write_bytes(p, i as u8, size) };
            held.push((p, layout, i as u8));
        }
        for &(p, layout, v) in &held {
            // SAFETY: as above.
            let s = unsafe { core::slice::from_raw_parts(p, layout.size()) };
            assert!(s.iter().all(|&b| b == v), "a block was overwritten");
        }
        for (p, layout, _) in held {
            // SAFETY: allocated above with `layout`.
            unsafe { a.dealloc(0, p, layout) };
        }
        let s = a.stats();
        assert_eq!((s.used, s.live), (0, 0));
        a.reclaim();
        assert_eq!(a.stats().slabs, 0);
        // TLSF rounds a request up to its list, so up to a sixteenth of
        // the largest free block cannot be had in one piece.
        assert!(a.regions.largest_free() > (8 << 20) / 16 * 15 - 4096);
    }

    #[test]
    fn realloc_keeps_contents() {
        let (a, _mem) = with_heap(4);
        let mut layout = Layout::from_size_align(10, 8).unwrap();
        // SAFETY: CPU 0 is used by this thread alone.
        let mut p = unsafe { a.alloc(0, layout) };
        // SAFETY: `p` holds 10 bytes.
        unsafe { ptr::copy_nonoverlapping(b"0123456789".as_ptr(), p, 10) };
        for n in [12, 16, 200, 5000, 40000, 100, 11] {
            // SAFETY: `p` was allocated with `layout`.
            let q = unsafe { a.realloc(0, p, layout, n) };
            assert!(!q.is_null());
            if n <= 16 && layout.size() <= 16 {
                assert_eq!(q, p, "a block that stays in its class moves");
            }
            // SAFETY: `q` holds at least 10 bytes.
            assert_eq!(unsafe { core::slice::from_raw_parts(q, 10) }, b"0123456789");
            p = q;
            layout = Layout::from_size_align(n, 8).unwrap();
            assert_eq!(a.stats().used, n);
        }
        // SAFETY: as above.
        unsafe { a.dealloc(0, p, layout) };
        assert_eq!(a.stats().used, 0);
    }

    /// A small xorshift, so the mixes are the same on every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn layout(&mut self) -> Layout {
            let size = match self.next() % 10 {
                0..=5 => 1 + self.next() % 256,
                6..=8 => 1 + self.next() % 16384,
                _ => 1 + self.next() % 200_000,
            } as usize;
            let align = 1 << (self.next() % 8);
            Layout::from_size_align(size, align).unwrap()
        }
    }

    /// Random mixes on several threads, each its own CPU, with blocks
    /// freed on other CPUs than they came from.
    #[test]
    fn random_mix_on_many_cpus() {
        let (a, _mem) = with_heap(64);
        let a = &a;
        let handoff = std::sync::Mutex::new(Vec::<(usize, Layout)>::new());
        let handoff = &handoff;
        std::thread::scope(|s| {
            for cpu in 0..4 {
                s.spawn(move || {
                    let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ (cpu as u64 + 1));
                    let mut held: Vec<(usize, Layout, u8)> = Vec::new();
                    for i in 0..20_000 {
                        if held.len() < 200 && !r.next().is_multiple_of(3) {
                            let l = r.layout();
                            // SAFETY: this thread alone is CPU `cpu`.
                            let p = unsafe { a.alloc(cpu, l) };
                            assert!(!p.is_null(), "out of memory");
                            assert_eq!(p as usize % l.align(), 0);
                            // SAFETY: `p` holds `l.size()` bytes.
                            unsafe { ptr::write_bytes(p, i as u8, l.size()) };
                            held.push((p as usize, l, i as u8));
                        } else if !held.is_empty() {
                            let (p, l, v) = held.swap_remove((r.next() as usize) % held.len());
                            // SAFETY: as above.
                            let b =
                                unsafe { core::slice::from_raw_parts(p as *const u8, l.size()) };
                            assert!(b.iter().all(|&x| x == v), "a block was overwritten");
                            if r.next().is_multiple_of(4) {
                                handoff.lock().unwrap().push((p, l));
                            } else {
                                // SAFETY: allocated above with `l`.
                                unsafe { a.dealloc(cpu, p as *mut u8, l) };
                            }
                        }
                        if i % 64 == 0 {
                            let theirs: Vec<_> = handoff.lock().unwrap().drain(..).collect();
                            for (p, l) in theirs {
                                // SAFETY: allocated by another CPU with `l`.
                                unsafe { a.dealloc(cpu, p as *mut u8, l) };
                            }
                        }
                        if cpu == 3 && i % 5000 == 0 {
                            a.reclaim();
                        }
                    }
                    for (p, l, _) in held {
                        // SAFETY: as above.
                        unsafe { a.dealloc(cpu, p as *mut u8, l) };
                    }
                });
            }
        });
        for (p, l) in handoff.lock().unwrap().drain(..) {
            // SAFETY: as above.
            unsafe { a.dealloc(0, p as *mut u8, l) };
        }
        let s = a.stats();
        assert_eq!((s.used, s.live), (0, 0), "{s:?}");
        assert!(s.peak > 0 && s.allocs > 0);
        a.reclaim();
        let s = a.stats();
        assert_eq!((s.cached, s.slabs), (0, 0), "{s:?}");
        // Everything coalesced back: no fragmentation left behind.
        assert!(a.regions.largest_free() > (64 << 20) / 16 * 15 - 4096);
    }

    /// Filling the heap ends in null, not a crash, and what the caches
    /// hold is given back first.
    #[test]
    fn exhaustion_is_a_clean_failure() {
        let (a, _mem) = with_heap(2);
        let mut held = Vec::new();
        let small = Layout::from_size_align(64, 8).unwrap();
        loop {
            // SAFETY: CPU 0 is used by this thread alone.
            let p = unsafe { a.alloc(0, small) };
            if p.is_null() {
                break;
            }
            held.push(p);
        }
        assert!(held.len() > 20_000, "only {} blocks", held.len());
        // Free them on CPU 1, where they stay cached; CPU 0 must still be
        // able to get a large block, by draining CPU 1.
        for p in held.drain(..) {
            // SAFETY: allocated above.
            unsafe { a.dealloc(1, p, small) };
        }
        let big = Layout::from_size_align(1 << 20, 16).unwrap();
        // SAFETY: as above.
        let p = unsafe { a.alloc(0, big) };
        assert!(!p.is_null(), "{:?}", a.stats());
        // SAFETY: as above.
        unsafe { a.dealloc(0, p, big) };
        let huge = Layout::from_size_align(4 << 20, 16).unwrap();
        // SAFETY: as above.
        assert!(unsafe { a.alloc(0, huge) }.is_null());
        assert_eq!(a.stats().used, 0);
    }

    /// A heap made of two regions uses both, and frees go home.
    /// A region extended at its top: slabs in the new piece are found
    /// through its own map, and everything goes back.
    #[test]
    fn extended_region() {
        let mut mem: Vec<u128> = vec![0; (2 << 20) / 16];
        let base: *mut u8 = mem.as_mut_ptr().cast();
        let a = Alloc::new();
        // SAFETY: the memory is ours; the second half is added later.
        unsafe {
            a.regions().init(&[(base, 1 << 20)]);
            assert!(!a.regions().extend(base.add(4096), 1 << 20));
            assert!(a.regions().extend(base.add(1 << 20), 1 << 20));
        }
        assert_eq!(a.stats().size, 2 << 20);
        let mut held = Vec::new();
        let mut r = Rng(7);
        loop {
            let l = Layout::from_size_align(1 + (r.next() % 3000) as usize, 8).unwrap();
            // SAFETY: CPU 0 is used by this thread alone.
            let p = unsafe { a.alloc(0, l) };
            if p.is_null() {
                break;
            }
            held.push((p, l));
        }
        let top = held.iter().map(|&(p, _)| p as usize).max().unwrap();
        assert!(
            top >= base as usize + (1 << 20),
            "the extension was not used"
        );
        for (p, l) in held {
            // SAFETY: allocated above with `l`.
            unsafe { a.dealloc(1, p, l) };
        }
        a.reclaim();
        let s = a.stats();
        assert_eq!((s.used, s.live, s.slabs), (0, 0, 0), "{s:?}");
    }

    /// Long-lived small blocks scattered among short-lived ones pin some
    /// slabs, but the large heap keeps most of its room in one piece.
    #[test]
    fn fragmentation_stays_bounded() {
        let (a, _mem) = with_heap(64);
        let mut r = Rng(11);
        let (mut keep, mut gone) = (Vec::new(), Vec::new());
        for i in 0..40_000 {
            let l = Layout::from_size_align(1 + (r.next() % 2048) as usize, 8).unwrap();
            // SAFETY: CPU 0 is used by this thread alone.
            let p = unsafe { a.alloc(0, l) };
            assert!(!p.is_null());
            if i % 50 == 0 {
                keep.push((p, l));
            } else {
                gone.push((p, l));
            }
        }
        for (p, l) in gone {
            // SAFETY: allocated above with `l`.
            unsafe { a.dealloc(0, p, l) };
        }
        a.reclaim();
        let s = a.stats();
        let largest = a.regions.largest_free();
        // The worst case for slabs: 800 blocks of about 1 KiB, every
        // fiftieth allocated, pin about 10 MiB of slabs, and the large
        // heap keeps 18 MiB in one piece; a first-fit heap would keep the
        // 23 MiB never touched.
        assert!(s.slabs <= 16 << 20, "{s:?}");
        assert!(largest > 16 << 20, "largest {largest}, {s:?}");
        for (p, l) in keep {
            // SAFETY: as above.
            unsafe { a.dealloc(0, p, l) };
        }
        a.reclaim();
        assert_eq!(a.stats().slabs, 0);
    }

    #[test]
    fn two_regions() {
        let mut m1: Vec<u128> = vec![0; (1 << 20) / 16];
        let mut m2: Vec<u128> = vec![0; (1 << 20) / 16];
        let a = Alloc::new();
        // SAFETY: the memory is ours.
        unsafe {
            a.regions().init(&[
                (m1.as_mut_ptr().cast(), 1 << 20),
                (ptr::null_mut(), 0),
                (m2.as_mut_ptr().cast(), 1 << 20),
            ])
        };
        let l = Layout::from_size_align(600 << 10, 16).unwrap();
        // SAFETY: CPU 0 is used by this thread alone.
        let (p, q) = unsafe { (a.alloc(0, l), a.alloc(0, l)) };
        assert!(!p.is_null() && !q.is_null());
        let in1 = |x: *mut u8| {
            (m1.as_ptr() as usize..m1.as_ptr() as usize + (1 << 20)).contains(&(x as usize))
        };
        assert_ne!(in1(p), in1(q));
        // SAFETY: as above.
        unsafe { (a.dealloc(0, p, l), a.dealloc(0, q, l)) };
        assert_eq!(a.stats().size, 2 << 20);
    }
}
