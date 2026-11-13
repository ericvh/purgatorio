//! Executable memory written at run time: the code region.
//!
//! The kernel's maps are W^X: text is read-only and executable, and
//! everything written at run time (the heap, the stacks) is never
//! executable. The AOT engine (`purg-vm-aot`, `docs/perf.md`) loads
//! native code at run time, after verifying it, so it needs memory that is
//! written once and then executed. [`load`] gives it that, without ever
//! making one mapping both writable and executable:
//!
//! 1. The pages come from 2 MiB chunks of the heap. A chunk's alias in
//!    the linear map is made read-only while the code region owns it (the
//!    kernel's runtime map uses 2 MiB blocks at most, so this changes one
//!    block that nothing else uses), so the code cannot be changed through
//!    the linear map either.
//! 2. The image gets a range of [`WINDOW`], a part of the high half that
//!    only this module maps. Its pages are mapped there read-write and
//!    never executable, the bytes are copied in, and the data cache is
//!    cleaned to the point of unification.
//! 3. The pages are remapped read-only and executable, the old entries
//!    are invalidated in every CPU's TLB, and the instruction cache is
//!    invalidated over the range. Every CPU then executes an `isb` before
//!    it first runs the code ([`sync`]).
//!
//! Dropping a [`Code`] unmaps its pages and gives them back; a chunk with
//! no pages in use goes back to the heap, read-write again.
//!
//! The pages of an image need not be contiguous: the window maps each of
//! them where it is. Tables for the window come from the heap and stay.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::asm;

use spin::Mutex;

use crate::exception::IrqGuard;
use crate::mmu::{self, AddressSpace, KERNEL_VBASE, PAGE_SIZE, Perm};

/// The part of the high half the code region maps: 1 GiB at the start
/// of the upper half of TTBR1's range, far from the linear map.
pub const WINDOW: core::ops::Range<usize> = 0xffff_8000_0000_0000..0xffff_8000_4000_0000;

/// The heap is asked for this much at a time.
const CHUNK: usize = 2 << 20;
const CHUNK_PAGES: usize = CHUNK / PAGE_SIZE;

/// Why code could not be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The heap has no 2 MiB chunk, or no page for a table.
    NoMemory,
    /// The window is full.
    NoSpace,
    /// The linear map does not hold a chunk as one 2 MiB block, so its
    /// alias could not be made read-only alone.
    Unsplit,
    /// An empty image.
    Empty,
}

impl Error {
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::NoMemory => "no memory for code",
            Error::NoSpace => "code region full",
            Error::Unsplit => "code memory is not a 2 MiB block",
            Error::Empty => "empty code",
        }
    }
}

/// A chunk of the heap, and which of its pages are in use.
struct Chunk {
    /// Its address in the linear map.
    va: usize,
    used: [u64; CHUNK_PAGES / 64],
}

impl Chunk {
    fn in_use(&self) -> usize {
        self.used.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn take(&mut self) -> Option<usize> {
        for (i, w) in self.used.iter_mut().enumerate() {
            if *w != u64::MAX {
                let bit = w.trailing_ones() as usize;
                *w |= 1 << bit;
                return Some(self.va + (i * 64 + bit) * PAGE_SIZE);
            }
        }
        None
    }
}

struct State {
    chunks: Vec<Chunk>,
    /// Free ranges of the window, `(first page, pages)`, in order.
    free: Vec<(usize, usize)>,
    /// Pages mapped now, for reports.
    pages: usize,
}

static STATE: Mutex<State> = Mutex::new(State {
    chunks: Vec::new(),
    free: Vec::new(),
    pages: 0,
});

/// Code loaded by [`load`]: read-only and executable at [`Code::as_ptr`]
/// until it is dropped.
pub struct Code {
    va: usize,
    len: usize,
    /// The physical page behind each page of the range.
    pages: Vec<usize>,
}

impl Code {
    pub fn as_ptr(&self) -> *const u8 {
        self.va as *const u8
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The code as bytes.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the range is mapped readable for as long as `self` lives
        // and nothing writes it.
        unsafe { core::slice::from_raw_parts(self.as_ptr(), self.len) }
    }
}

impl Drop for Code {
    fn drop(&mut self) {
        let _g = IrqGuard::new();
        let mut st = STATE.lock();
        for i in 0..self.pages.len() {
            let va = self.va + i * PAGE_SIZE;
            if let Some(e) = page_entry(va) {
                // SAFETY: the entry is this code's own page in the window.
                unsafe { e.write_volatile(0) };
            }
        }
        flush_tlb(self.va, self.pages.len());
        for &pa in &self.pages {
            st.give_back(mmu::phys_to_virt(pa));
        }
        st.pages -= self.pages.len();
        st.free_range(page_index(self.va), self.pages.len());
    }
}

/// What the code region holds: pages mapped and chunks taken.
pub fn stats() -> (usize, usize) {
    let _g = IrqGuard::new();
    let st = STATE.lock();
    (st.pages, st.chunks.len())
}

/// Whether `ptr..ptr + len` is inside [`WINDOW`]: memory only [`load`]
/// maps, and only once it is executable.
pub fn contains(ptr: *const u8, len: usize) -> bool {
    let start = ptr as usize;
    start >= WINDOW.start && start.checked_add(len).is_some_and(|end| end <= WINDOW.end)
}

/// Copies `image` into new executable memory (see the module docs).
pub fn load(image: &[u8]) -> Result<Code, Error> {
    if image.is_empty() {
        return Err(Error::Empty);
    }
    let n = image.len().div_ceil(PAGE_SIZE);
    let (va, pages) = {
        let _g = IrqGuard::new();
        STATE.lock().map(n)?
    };
    let code = Code {
        va,
        len: image.len(),
        pages,
    };
    // SAFETY: the range was just mapped read-write for this code alone.
    let dst = unsafe { core::slice::from_raw_parts_mut(va as *mut u8, n * PAGE_SIZE) };
    dst[..image.len()].copy_from_slice(image);
    dst[image.len()..].fill(0);
    clean_dcache(va, n * PAGE_SIZE);
    for (i, &pa) in code.pages.iter().enumerate() {
        let e = page_entry(va + i * PAGE_SIZE).expect("a mapped page has an entry");
        // SAFETY: the entry is this code's own page; only the permissions
        // change, so no break-before-make is needed.
        unsafe { e.write_volatile(mmu::page_descriptor(pa, Perm::Text)) };
    }
    flush_tlb(va, n);
    invalidate_icache(va, n * PAGE_SIZE);
    Ok(code)
}

/// Makes this CPU see code loaded by another one: an `isb`, so that no
/// instruction fetched before the other CPU's invalidation runs.
pub fn sync() {
    // SAFETY: a barrier.
    unsafe { asm!("isb", options(nostack, preserves_flags)) };
}

impl State {
    /// Maps `n` fresh pages read-write at a free range of the window.
    fn map(&mut self, n: usize) -> Result<(usize, Vec<usize>), Error> {
        let first = self.alloc_range(n)?;
        let va = WINDOW.start + first * PAGE_SIZE;
        let mut pages = Vec::with_capacity(n);
        let mut new_table = new_table;
        let r = (|| {
            for i in 0..n {
                let page = self.take_page()?;
                pages.push(mmu::virt_to_phys(page));
                let space = AddressSpace::from_root(mmu::ttbr1());
                // SAFETY: the kernel's tables are in the linear map, and
                // `new_table` hands out zeroed heap pages.
                let e = unsafe {
                    space.page_entry_with(KERNEL_VBASE, va + i * PAGE_SIZE, &mut new_table)
                }
                .map_err(|_| Error::NoMemory)?;
                // SAFETY: a free page of the window: its entry is invalid,
                // so no TLB holds it.
                unsafe {
                    e.write_volatile(mmu::page_descriptor(
                        mmu::virt_to_phys(page),
                        Perm::ReadWrite,
                    ))
                };
            }
            Ok(())
        })();
        // SAFETY: a barrier, so the table walker sees the new entries.
        unsafe { asm!("dsb ishst", "isb", options(nostack, preserves_flags)) };
        if let Err(e) = r {
            for i in 0..pages.len() {
                if let Some(e) = page_entry(va + i * PAGE_SIZE) {
                    // SAFETY: as above.
                    unsafe { e.write_volatile(0) };
                }
            }
            flush_tlb(va, pages.len());
            for &pa in &pages {
                self.give_back(mmu::phys_to_virt(pa));
            }
            self.free_range(first, n);
            return Err(e);
        }
        self.pages += n;
        Ok((va, pages))
    }

    fn alloc_range(&mut self, n: usize) -> Result<usize, Error> {
        if self.free.is_empty() && self.pages == 0 {
            self.free = alloc::vec![(0, (WINDOW.end - WINDOW.start) / PAGE_SIZE)];
        }
        let i = self
            .free
            .iter()
            .position(|&(_, len)| len >= n)
            .ok_or(Error::NoSpace)?;
        let (start, len) = self.free[i];
        if len == n {
            self.free.remove(i);
        } else {
            self.free[i] = (start + n, len - n);
        }
        Ok(start)
    }

    fn free_range(&mut self, start: usize, n: usize) {
        let i = self.free.partition_point(|&(s, _)| s < start);
        self.free.insert(i, (start, n));
        // Merge with the next range, then with the one before.
        if i + 1 < self.free.len() && start + n == self.free[i + 1].0 {
            self.free[i].1 += self.free[i + 1].1;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == start {
            self.free[i - 1].1 += self.free[i].1;
            self.free.remove(i);
        }
    }

    /// A free page, in the linear map, from a chunk with room or a new one.
    fn take_page(&mut self) -> Result<usize, Error> {
        for c in &mut self.chunks {
            if let Some(p) = c.take() {
                return Ok(p);
            }
        }
        let layout = Layout::from_size_align(CHUNK, CHUNK).map_err(|_| Error::NoMemory)?;
        // SAFETY: a non-zero-size layout.
        let va = unsafe { alloc::alloc::alloc(layout) } as usize;
        if va == 0 {
            return Err(Error::NoMemory);
        }
        if let Err(e) = set_alias_read_only(va, true) {
            // SAFETY: allocated just above with this layout, never used.
            unsafe { alloc::alloc::dealloc(va as *mut u8, layout) };
            return Err(e);
        }
        let mut c = Chunk {
            va,
            used: [0; CHUNK_PAGES / 64],
        };
        let p = c.take().ok_or(Error::NoMemory)?;
        self.chunks.push(c);
        Ok(p)
    }

    /// Returns a page (by its linear address); frees its chunk if that
    /// was the chunk's last page in use.
    fn give_back(&mut self, page: usize) {
        let Some(i) = self
            .chunks
            .iter()
            .position(|c| (c.va..c.va + CHUNK).contains(&page))
        else {
            return;
        };
        let bit = (page - self.chunks[i].va) / PAGE_SIZE;
        self.chunks[i].used[bit / 64] &= !(1 << (bit % 64));
        if self.chunks[i].in_use() == 0 {
            let c = self.chunks.swap_remove(i);
            // The chunk was read-only, so restoring it cannot fail.
            let _ = set_alias_read_only(c.va, false);
            let layout = Layout::from_size_align(CHUNK, CHUNK).expect("chunk layout");
            // SAFETY: allocated in `take_page` with this layout, writable
            // again through the linear map, and no page of it is mapped
            // in the window.
            unsafe { alloc::alloc::dealloc(c.va as *mut u8, layout) };
        }
    }
}

fn page_index(va: usize) -> usize {
    (va - WINDOW.start) / PAGE_SIZE
}

/// The window's entry for `va`, if its tables exist.
fn page_entry(va: usize) -> Option<*mut u64> {
    let space = AddressSpace::from_root(mmu::ttbr1());
    // SAFETY: the kernel's tables are in the linear map.
    unsafe { space.entry_at(KERNEL_VBASE, va, 3) }
}

/// A zeroed heap page for a table, by physical address. Tables are never
/// freed.
fn new_table() -> Option<usize> {
    let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).ok()?;
    // SAFETY: a non-zero-size layout.
    let va = unsafe { alloc::alloc::alloc_zeroed(layout) };
    (!va.is_null()).then(|| mmu::virt_to_phys(va as usize))
}

/// Makes the linear map's 2 MiB block at `va` read-only, or read-write
/// again, in every CPU's TLB.
fn set_alias_read_only(va: usize, ro: bool) -> Result<(), Error> {
    let space = AddressSpace::from_root(mmu::ttbr1());
    // SAFETY: the kernel's tables are in the linear map.
    let e = unsafe { space.entry_at(KERNEL_VBASE, va, 2) }.ok_or(Error::Unsplit)?;
    // SAFETY: the entry is live; it maps a chunk only the code region
    // uses, and only its access permission changes, which needs no
    // break-before-make.
    unsafe {
        let d = e.read_volatile();
        if !mmu::is_block(d) {
            return Err(Error::Unsplit);
        }
        e.write_volatile(mmu::with_read_only(d, ro));
    }
    flush_tlb(va, 1);
    Ok(())
}

/// Invalidates the TLB entries for `n` pages from `va` on every CPU, then
/// waits for that. A block's entry goes with any address in it.
fn flush_tlb(va: usize, n: usize) {
    // SAFETY: TLB maintenance and barriers only.
    unsafe {
        asm!("dsb ishst", options(nostack, preserves_flags));
        if n > 64 {
            asm!("tlbi vmalle1is", options(nostack, preserves_flags));
        } else {
            for i in 0..n {
                let page = ((va + i * PAGE_SIZE) >> 12) as u64 & ((1 << 44) - 1);
                asm!("tlbi vaae1is, {}", in(reg) page, options(nostack, preserves_flags));
            }
        }
        asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
}

/// Cleans the data cache over `len` bytes from `va` to the point of
/// unification, where instruction fetches see it.
fn clean_dcache(va: usize, len: usize) {
    let line = crate::cache::dline();
    let mut a = va & !(line - 1);
    // SAFETY: cache maintenance over memory mapped at `va`.
    unsafe {
        while a < va + len {
            asm!("dc cvau, {}", in(reg) a, options(nostack, preserves_flags));
            a += line;
        }
        asm!("dsb ish", options(nostack, preserves_flags));
    }
}

/// Invalidates the instruction cache over `len` bytes from `va` on every
/// CPU, then synchronizes this one.
fn invalidate_icache(va: usize, len: usize) {
    let line = crate::cache::iline();
    let mut a = va & !(line - 1);
    // SAFETY: cache maintenance over memory mapped at `va`.
    unsafe {
        while a < va + len {
            asm!("ic ivau, {}", in(reg) a, options(nostack, preserves_flags));
            a += line;
        }
        asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
}
