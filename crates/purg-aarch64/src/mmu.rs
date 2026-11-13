//! Stage-1 EL1 page tables: an identity map in TTBR0 for turning the MMU on,
//! and the kernel high half in TTBR1.
//!
//! Translation regime: 4 KiB granule, 48-bit virtual addresses
//! (`T0SZ = T1SZ = 16`, four levels), inner-shareable write-back walks.
//! `MAIR_EL1` attr0 is Device-nGnRnE and attr1 is Normal write-back.
//!
//! The high half is a linear map: virtual `KERNEL_VBASE + p` is physical
//! `p`, for RAM and for the device window alike, so [`phys_to_virt`] and
//! [`virt_to_phys`] are additions. The kernel image is linked inside this
//! map at `KERNEL_VBASE + <load address>`, with per-section permissions.
//!
//! The table builder ([`TableAlloc`], [`AddressSpace`], [`build_kernel_map`])
//! is plain code over raw pointers. It runs on the host in unit tests, and
//! in the kernel both before the MMU is on (tables addressed physically,
//! `access = 0`) and after (tables addressed through the linear map,
//! `access = KERNEL_VBASE`). It does not index slices, so it has no bounds
//! check panics, and it keeps no pointers in statics: both matter before
//! the MMU is on, when only PC-relative addressing gives correct addresses.
//! The functions that touch system registers exist only on
//! `aarch64-unknown-none`.

/// The base of the kernel high half (TTBR1). Physical `p` is at
/// `KERNEL_VBASE + p`.
pub const KERNEL_VBASE: usize = 0xffff_0000_0000_0000;

/// The translation granule. Page tables use 4 KiB pages whatever the host
/// page size; this is the kernel's own page size.
pub const PAGE_SIZE: usize = 4096;

const L1_BLOCK: usize = 1 << 30;
const L2_BLOCK: usize = 1 << 21;
const ENTRIES: usize = 512;

/// The kernel-virtual address of physical `p` in the linear map.
pub const fn phys_to_virt(p: usize) -> usize {
    p.wrapping_add(KERNEL_VBASE)
}

/// The physical address of kernel-virtual `v`, which must be in the linear
/// map (the kernel image is too).
pub const fn virt_to_phys(v: usize) -> usize {
    v.wrapping_sub(KERNEL_VBASE)
}

/// `MAIR_EL1`: attr0 = Device-nGnRnE (0x00), attr1 = Normal, inner and
/// outer write-back read/write-allocate (0xff).
pub const MAIR: u64 = 0xff << 8;

/// `TCR_EL1` without `IPS`, which [`enable`] fills in from
/// `ID_AA64MMFR0_EL1.PARange`: T0SZ = T1SZ = 16, TG0 = 4K, TG1 = 4K,
/// write-back inner shareable walks for both halves, ASID 8 bits, A1 = 0.
pub const TCR: u64 = 16          // T0SZ
    | (1 << 8)                   // IRGN0 = WB RA/WA
    | (1 << 10)                  // ORGN0 = WB RA/WA
    | (3 << 12)                  // SH0 = inner shareable
    | (16 << 16)                 // T1SZ
    | (1 << 24)                  // IRGN1
    | (1 << 26)                  // ORGN1
    | (3 << 28)                  // SH1
    | (2 << 30); // TG1 = 4K

// Descriptor bits.
const VALID: u64 = 1;
const TABLE: u64 = 0b11;
const BLOCK: u64 = 0b01;
const PAGE: u64 = 0b11;
const ATTR_DEVICE: u64 = 0 << 2;
const ATTR_NORMAL: u64 = 1 << 2;
const AP_RO: u64 = 1 << 7;
const SH_INNER: u64 = 3 << 8;
const AF: u64 = 1 << 10;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

/// What a mapping allows. Every mapping is EL1-only and never executable
/// from EL0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    /// Device-nGnRnE, read/write, never executable.
    Device,
    /// Normal memory, read-only and executable.
    Text,
    /// Normal memory, read-only, never executable.
    ReadOnly,
    /// Normal memory, read/write, never executable.
    ReadWrite,
    /// Normal memory, read/write and executable: the boot identity map only.
    ReadWriteExec,
}

impl Perm {
    const fn bits(self) -> u64 {
        let normal = ATTR_NORMAL | SH_INNER | AF | UXN;
        match self {
            Perm::Device => ATTR_DEVICE | AF | PXN | UXN,
            Perm::Text => normal | AP_RO,
            Perm::ReadOnly => normal | AP_RO | PXN,
            Perm::ReadWrite => normal | PXN,
            Perm::ReadWriteExec => normal,
        }
    }
}

/// Why a mapping could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The table pool is used up.
    OutOfTables,
    /// An address or length is not page-aligned.
    Unaligned,
    /// Part of the range is already mapped.
    AlreadyMapped,
}

impl Error {
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::OutOfTables => "out of page tables",
            Error::Unaligned => "unaligned mapping",
            Error::AlreadyMapped => "range already mapped",
        }
    }
}

/// A physical address range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub base: usize,
    pub size: usize,
}

impl Region {
    pub const fn end(&self) -> usize {
        self.base + self.size
    }
}

/// A bump allocator of zeroed 4 KiB table pages from a physical pool.
pub struct TableAlloc {
    next: usize,
    end: usize,
    access: usize,
    /// Whether [`AddressSpace::map`] may use 1 GiB blocks.
    gib: bool,
}

impl TableAlloc {
    /// Hands out pages of the physical pool `pool`, which this code writes
    /// at `physical + access`: 0 while the MMU is off or under an identity
    /// map, [`KERNEL_VBASE`] in the high half.
    ///
    /// # Safety
    /// `pool` must be page-aligned memory that nothing else uses for as long
    /// as the tables built from it are live, and writable at `access`.
    pub const unsafe fn new(pool: Region, access: usize) -> TableAlloc {
        TableAlloc {
            next: pool.base,
            end: pool.base + pool.size,
            access,
            gib: true,
        }
    }

    /// Makes [`AddressSpace::map`] use blocks of at most 2 MiB with these
    /// tables. The kernel's runtime map is built this way, so that a
    /// 2 MiB block of RAM can change its permissions alone
    /// ([`crate::code`]) without splitting a live 1 GiB block.
    pub const fn without_gib_blocks(mut self) -> TableAlloc {
        self.gib = false;
        self
    }

    /// Pages left in the pool.
    pub const fn remaining(&self) -> usize {
        (self.end - self.next) / PAGE_SIZE
    }

    fn alloc(&mut self) -> Result<usize, Error> {
        if !self.next.is_multiple_of(PAGE_SIZE) {
            return Err(Error::Unaligned);
        }
        if self.end.saturating_sub(self.next) < PAGE_SIZE {
            return Err(Error::OutOfTables);
        }
        let pa = self.next;
        self.next += PAGE_SIZE;
        let t = self.table(pa);
        for i in 0..ENTRIES {
            // SAFETY: `new` makes the pool writable at `access`; `i` is in
            // the page. Volatile word stores stay aligned even while the MMU
            // is off and all memory is Device.
            unsafe { t.add(i).write_volatile(0) };
        }
        Ok(pa)
    }

    fn table(&self, pa: usize) -> *mut u64 {
        pa.wrapping_add(self.access) as *mut u64
    }
}

/// A four-level translation table tree; its root goes in a TTBR.
pub struct AddressSpace {
    root: usize,
}

const fn index(va: usize, level: usize) -> usize {
    (va >> (39 - 9 * level)) & (ENTRIES - 1)
}

impl AddressSpace {
    pub fn new(alloc: &mut TableAlloc) -> Result<AddressSpace, Error> {
        Ok(AddressSpace {
            root: alloc.alloc()?,
        })
    }

    /// The tree whose level-0 table is at physical `root` (from a TTBR).
    pub const fn from_root(root: usize) -> AddressSpace {
        AddressSpace { root }
    }

    /// The physical address of the level-0 table, for `TTBRn_EL1`.
    pub const fn root(&self) -> usize {
        self.root
    }

    /// Maps `len` bytes at virtual `va` to physical `pa` with `perm`,
    /// using 1 GiB and 2 MiB blocks where the alignment allows and 4 KiB
    /// pages elsewhere.
    pub fn map(
        &mut self,
        alloc: &mut TableAlloc,
        va: usize,
        pa: usize,
        len: usize,
        perm: Perm,
    ) -> Result<(), Error> {
        if !(va | pa | len).is_multiple_of(PAGE_SIZE) {
            return Err(Error::Unaligned);
        }
        let (mut va, mut pa, mut len) = (va, pa, len);
        while len > 0 {
            let fits = |size: usize| (va | pa).is_multiple_of(size) && len >= size;
            let (level, size, kind) = if alloc.gib && fits(L1_BLOCK) {
                (1, L1_BLOCK, BLOCK)
            } else if fits(L2_BLOCK) {
                (2, L2_BLOCK, BLOCK)
            } else {
                (3, PAGE_SIZE, PAGE)
            };
            let entry = self.entry(alloc, va, level)?;
            // SAFETY: `entry` points into a table from `alloc`'s pool.
            unsafe {
                if entry.read_volatile() & VALID != 0 {
                    return Err(Error::AlreadyMapped);
                }
                entry.write_volatile((pa as u64 & ADDR_MASK) | perm.bits() | kind);
            }
            va = va.wrapping_add(size);
            pa += size;
            len -= size;
        }
        Ok(())
    }

    /// The entry for `va` at `level`, creating tables above it as needed.
    fn entry(&self, alloc: &mut TableAlloc, va: usize, level: usize) -> Result<*mut u64, Error> {
        let mut table = self.root;
        for l in 0..level {
            let e = alloc.table(table).wrapping_add(index(va, l));
            // SAFETY: `e` is an entry of a table from `alloc`'s pool.
            let d = unsafe { e.read_volatile() };
            table = if d & VALID == 0 {
                let t = alloc.alloc()?;
                // SAFETY: as above.
                unsafe { e.write_volatile(t as u64 | TABLE) };
                t
            } else if d & 0b11 == TABLE {
                (d & ADDR_MASK) as usize
            } else {
                return Err(Error::AlreadyMapped);
            };
        }
        Ok(alloc.table(table).wrapping_add(index(va, level)))
    }

    /// Walks the tables for `va` (written at `physical + access`) and
    /// returns the physical address and the leaf descriptor, if mapped.
    ///
    /// # Safety
    /// The tables must be valid and readable at `physical + access`.
    pub unsafe fn lookup(&self, access: usize, va: usize) -> Option<(usize, u64)> {
        let mut table = self.root;
        for level in 0..4 {
            let e = table.wrapping_add(access) as *const u64;
            // SAFETY: the caller guarantees the tables are readable at `access`.
            let d = unsafe { e.wrapping_add(index(va, level)).read_volatile() };
            if d & VALID == 0 {
                return None;
            }
            let size = 1usize << (39 - 9 * level);
            if level == 3 || d & 0b11 == BLOCK {
                if level == 0 {
                    return None;
                }
                let pa = (d & ADDR_MASK) as usize & !(size - 1);
                return Some((pa | (va & (size - 1)), d));
            }
            table = (d & ADDR_MASK) as usize;
        }
        None
    }
}

impl AddressSpace {
    /// The entry for `va` at `level` (2 for a 2 MiB block, 3 for a page),
    /// if the tables above it exist. The entry itself may be invalid.
    ///
    /// # Safety
    /// The tables must be valid and readable at `physical + access`.
    pub unsafe fn entry_at(&self, access: usize, va: usize, level: usize) -> Option<*mut u64> {
        let mut table = self.root;
        for l in 0..level {
            let e = table.wrapping_add(access) as *const u64;
            // SAFETY: the caller guarantees the tables are readable.
            let d = unsafe { e.wrapping_add(index(va, l)).read_volatile() };
            if d & 0b11 != TABLE {
                return None;
            }
            table = (d & ADDR_MASK) as usize;
        }
        Some((table.wrapping_add(access) as *mut u64).wrapping_add(index(va, level)))
    }

    /// The level-3 (page) entry for `va`, making the tables above it with
    /// `new_table`, which returns the physical address of a zeroed page,
    /// or `None` when there is no memory.
    ///
    /// # Safety
    /// The tables must be valid and writable at `physical + access`, and
    /// so must the pages `new_table` returns. The caller orders the new
    /// table entries before any walk that needs them (`dsb ishst`).
    pub unsafe fn page_entry_with(
        &self,
        access: usize,
        va: usize,
        new_table: &mut dyn FnMut() -> Option<usize>,
    ) -> Result<*mut u64, Error> {
        let mut table = self.root;
        for l in 0..3 {
            let e = (table.wrapping_add(access) as *mut u64).wrapping_add(index(va, l));
            // SAFETY: the caller guarantees the tables are writable.
            let d = unsafe { e.read_volatile() };
            table = if d & VALID == 0 {
                let t = new_table().ok_or(Error::OutOfTables)?;
                // SAFETY: as above.
                unsafe { e.write_volatile(t as u64 | TABLE) };
                t
            } else if d & 0b11 == TABLE {
                (d & ADDR_MASK) as usize
            } else {
                return Err(Error::AlreadyMapped);
            };
        }
        Ok((table.wrapping_add(access) as *mut u64).wrapping_add(index(va, 3)))
    }
}

/// A level-3 descriptor mapping the 4 KiB page at physical `pa`.
pub const fn page_descriptor(pa: usize, perm: Perm) -> u64 {
    (pa as u64 & ADDR_MASK) | perm.bits() | PAGE
}

/// True if `d` is a valid block descriptor (at level 1 or 2).
pub const fn is_block(d: u64) -> bool {
    d & 0b11 == BLOCK
}

/// Leaf descriptor `d` made read-only (`true`) or read-write, nothing
/// else changed.
pub const fn with_read_only(d: u64, ro: bool) -> u64 {
    if ro { d | AP_RO } else { d & !AP_RO }
}

/// True if leaf descriptor `d` (from [`AddressSpace::lookup`]) is
/// executable at EL1.
pub const fn executable(d: u64) -> bool {
    d & PXN == 0
}

/// True if leaf descriptor `d` is writable at EL1.
pub const fn writable(d: u64) -> bool {
    d & AP_RO == 0
}

/// The physical extent of the kernel image's sections, each page-aligned.
/// `text` is mapped RX, `rodata` R, `data` (data and bss) and `stack` RW.
/// The page below `stack` is left unmapped as a guard; it must be inside
/// the image (`image()` covers `text.base..stack.end()`).
#[derive(Clone, Copy, Debug)]
pub struct KernelImage {
    pub text: Region,
    pub rodata: Region,
    pub data: Region,
    pub stack: Region,
}

impl KernelImage {
    /// The whole image, from the start of text to the top of the stack.
    pub const fn image(&self) -> Region {
        Region {
            base: self.text.base,
            size: self.stack.end() - self.text.base,
        }
    }
}

/// The roots of a built kernel map.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct KernelMap {
    /// The identity map, for `TTBR0_EL1`.
    pub ttbr0: usize,
    /// The high half, for `TTBR1_EL1`.
    pub ttbr1: usize,
}

/// Builds the identity map and the high half for `ram`, the device window
/// `devices`, and the kernel `image` (which must lie inside one of `ram`).
///
/// TTBR0 identity-maps `devices` (Device) and all of `ram` (Normal RWX):
/// the boot code runs from it at its physical address while the MMU turns
/// on. TTBR1 maps the same ranges at [`phys_to_virt`]: `devices` as
/// Device, RAM outside the image as Normal RW, and the image section by
/// section.
pub fn build_kernel_map(
    alloc: &mut TableAlloc,
    ram: &[Region],
    devices: Region,
    image: &KernelImage,
) -> Result<KernelMap, Error> {
    let mut low = AddressSpace::new(alloc)?;
    let mut high = AddressSpace::new(alloc)?;
    low.map(
        alloc,
        devices.base,
        devices.base,
        devices.size,
        Perm::Device,
    )?;
    high.map(
        alloc,
        phys_to_virt(devices.base),
        devices.base,
        devices.size,
        Perm::Device,
    )?;
    let img = image.image();
    fn linear(
        high: &mut AddressSpace,
        alloc: &mut TableAlloc,
        base: usize,
        end: usize,
    ) -> Result<(), Error> {
        if end > base {
            high.map(alloc, phys_to_virt(base), base, end - base, Perm::ReadWrite)
        } else {
            Ok(())
        }
    }
    let mut i = 0;
    while i < ram.len() {
        // SAFETY: `i < ram.len()`; indexing without a bounds-check panic.
        let r = unsafe { *ram.as_ptr().add(i) };
        i += 1;
        low.map(alloc, r.base, r.base, r.size, Perm::ReadWriteExec)?;
        if img.base >= r.base && img.end() <= r.end() {
            linear(&mut high, alloc, r.base, img.base)?;
            linear(&mut high, alloc, img.end(), r.end())?;
        } else {
            linear(&mut high, alloc, r.base, r.end())?;
        }
    }
    let sections = [
        (image.text, Perm::Text),
        (image.rodata, Perm::ReadOnly),
        (image.data, Perm::ReadWrite),
        (image.stack, Perm::ReadWrite),
    ];
    let mut s = 0;
    while s < sections.len() {
        // SAFETY: `s < sections.len()`.
        let (r, perm) = unsafe { *sections.as_ptr().add(s) };
        s += 1;
        if r.size > 0 {
            high.map(alloc, phys_to_virt(r.base), r.base, r.size, perm)?;
        }
    }
    Ok(KernelMap {
        ttbr0: low.root(),
        ttbr1: high.root(),
    })
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod hw {
    use super::{KernelMap, MAIR, TCR};
    use core::arch::asm;

    /// `SCTLR_EL1` with the MMU and caches off: the ARMv8.0 RES1 bits only.
    pub const SCTLR_OFF: u64 = 0x30d0_0800;

    /// `SCTLR_EL1` with the MMU (M), data cache (C), instruction cache (I)
    /// and SP alignment checks (SA, SA0) on. Alignment checking (A) and
    /// WXN stay off.
    pub const SCTLR_ON: u64 = SCTLR_OFF | 1 | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 12);

    /// Loads `map` into TTBR0/TTBR1, programs MAIR and TCR (IPS from the
    /// CPU's physical address range), and turns on the MMU and caches.
    ///
    /// # Safety
    /// The caller must be executing from memory `map.ttbr0` identity-maps,
    /// with the MMU off, at EL1, and the tables must stay valid.
    pub unsafe fn enable(map: &KernelMap) {
        // SAFETY: the caller guarantees the tables and the identity map;
        // the barriers order the table writes before the walker sees them
        // and the TLB invalidation before the MMU turns on.
        unsafe {
            let mmfr0: u64;
            asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack));
            let ips = (mmfr0 & 0xf).min(5);
            asm!(
                "dsb ishst",
                "msr mair_el1, {mair}",
                "msr tcr_el1, {tcr}",
                "msr ttbr0_el1, {t0}",
                "msr ttbr1_el1, {t1}",
                "isb",
                "tlbi vmalle1",
                "ic iallu",
                "dsb ish",
                "isb",
                "msr sctlr_el1, {sctlr}",
                "isb",
                mair = in(reg) MAIR,
                tcr = in(reg) TCR | (ips << 32),
                t0 = in(reg) map.ttbr0,
                t1 = in(reg) map.ttbr1,
                sctlr = in(reg) SCTLR_ON,
                options(nostack),
            );
        }
    }

    /// Switches to new tables. Both must map the running code and stack
    /// at the same addresses as the current ones.
    ///
    /// # Safety
    /// As for [`enable`], except that the MMU is already on.
    pub unsafe fn switch(map: &KernelMap) {
        // SAFETY: the caller guarantees the new tables map the kernel the
        // same way, so nothing in use moves.
        unsafe {
            asm!(
                "dsb ishst",
                "msr ttbr0_el1, {t0}",
                "msr ttbr1_el1, {t1}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                t0 = in(reg) map.ttbr0,
                t1 = in(reg) map.ttbr1,
                options(nostack),
            );
        }
    }

    /// Invalidates the data cache over `r` (by VA = PA, so with the MMU
    /// off or under the identity map) to the point of coherency, so memory
    /// written with the MMU off is not shadowed by stale lines once it is
    /// read cacheably.
    ///
    /// # Safety
    /// Nothing in `r` may be dirty in the cache: the lines are discarded.
    pub unsafe fn invalidate_dcache(r: super::Region) {
        // SAFETY: the caller guarantees no dirty data is discarded.
        unsafe {
            let line = crate::cache::dline();
            let mut a = r.base & !(line - 1);
            asm!("dsb sy", options(nostack));
            while a < r.end() {
                asm!("dc ivac, {}", in(reg) a, options(nostack));
                a += line;
            }
            asm!("dsb sy", options(nostack));
        }
    }

    /// The current `TTBR1_EL1` table root.
    pub fn ttbr1() -> usize {
        let v: usize;
        // SAFETY: reading TTBR1 has no side effects.
        unsafe { asm!("mrs {}, ttbr1_el1", out(reg) v, options(nomem, nostack)) };
        v & super::ADDR_MASK as usize
    }

    /// Asks the MMU whether EL1 can read (`write = false`) or write `va`,
    /// with `AT S1E1R`/`AT S1E1W`.
    pub fn probe(va: usize, write: bool) -> bool {
        let par: u64;
        // SAFETY: address translation instructions only update PAR_EL1.
        unsafe {
            if write {
                asm!("at s1e1w, {va}", "isb", "mrs {par}, par_el1",
                    va = in(reg) va, par = out(reg) par, options(nostack));
            } else {
                asm!("at s1e1r, {va}", "isb", "mrs {par}, par_el1",
                    va = in(reg) va, par = out(reg) par, options(nostack));
            }
        }
        par & 1 == 0
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use hw::{SCTLR_OFF, SCTLR_ON, enable, invalidate_dcache, probe, switch, ttbr1};

#[cfg(test)]
mod tests {
    use super::*;

    /// A host buffer standing in for physical pages at `PA`.
    struct Pool {
        mem: Vec<u64>,
    }

    const PA: usize = 0x4000_0000;

    impl Pool {
        fn new(pages: usize) -> Pool {
            // One extra page so the buffer can be page-aligned.
            Pool {
                mem: vec![0; (pages + 1) * ENTRIES],
            }
        }
        fn alloc(&mut self, pages: usize) -> (TableAlloc, usize) {
            let p = self.mem.as_mut_ptr() as usize;
            let aligned = (p + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            let access = aligned.wrapping_sub(PA);
            let r = Region {
                base: PA,
                size: pages * PAGE_SIZE,
            };
            // SAFETY: the region is inside `mem`, which outlives the test.
            (unsafe { TableAlloc::new(r, access) }, access)
        }
    }

    fn look(a: &AddressSpace, access: usize, va: usize) -> Option<(usize, u64)> {
        // SAFETY: the tests' tables live in a `Pool` that outlives this.
        unsafe { a.lookup(access, va) }
    }

    fn image() -> KernelImage {
        let base = 0x4020_0000;
        KernelImage {
            text: Region { base, size: 0x3000 },
            rodata: Region {
                base: base + 0x3000,
                size: 0x2000,
            },
            data: Region {
                base: base + 0x5000,
                size: 0x1000,
            },
            stack: Region {
                base: base + 0x7000,
                size: 0x4000,
            },
        }
    }

    #[test]
    fn kernel_map() {
        let mut pool = Pool::new(16);
        let (mut alloc, access) = pool.alloc(16);
        let ram = [Region {
            base: 0x4000_0000,
            size: 512 << 20,
        }];
        let dev = Region {
            base: 0,
            size: 1 << 30,
        };
        let img = image();
        let map = build_kernel_map(&mut alloc, &ram, dev, &img).unwrap();
        let high = AddressSpace { root: map.ttbr1 };
        let low = AddressSpace { root: map.ttbr0 };

        // Device window.
        let (pa, d) = look(&high, access, phys_to_virt(0x0900_0000)).unwrap();
        assert_eq!(pa, 0x0900_0000);
        assert_eq!(d & (7 << 2), ATTR_DEVICE);
        assert!(!executable(d));
        // Identity map of RAM, executable for the boot switch.
        let (pa, d) = look(&low, access, 0x4020_0010).unwrap();
        assert_eq!(pa, 0x4020_0010);
        assert!(executable(d) && writable(d));
        // Kernel sections.
        let (pa, d) = look(&high, access, phys_to_virt(0x4020_0100)).unwrap();
        assert_eq!(pa, 0x4020_0100);
        assert!(executable(d) && !writable(d));
        let (_, d) = look(&high, access, phys_to_virt(0x4020_3000)).unwrap();
        assert!(!executable(d) && !writable(d));
        let (_, d) = look(&high, access, phys_to_virt(0x4020_5000)).unwrap();
        assert!(!executable(d) && writable(d));
        // The stack guard page is unmapped.
        assert!(look(&high, access, phys_to_virt(0x4020_6000)).is_none());
        let (_, d) = look(&high, access, phys_to_virt(0x4020_a000)).unwrap();
        assert!(!executable(d) && writable(d));
        // RAM below and above the image, through blocks.
        let (pa, d) = look(&high, access, phys_to_virt(0x4000_1234)).unwrap();
        assert_eq!(pa, 0x4000_1234);
        assert_eq!(d & 0b11, BLOCK);
        let (pa, d) = look(&high, access, phys_to_virt(0x5fff_fff8)).unwrap();
        assert_eq!(pa, 0x5fff_fff8);
        assert!(!executable(d) && writable(d));
        assert!(look(&high, access, phys_to_virt(0x6000_0000)).is_none());
        // Nothing low in TTBR1's own index space leaks into TTBR0.
        assert!(look(&low, access, 0x6000_0000).is_none());
    }

    #[test]
    fn errors() {
        let mut pool = Pool::new(4);
        let (mut alloc, _) = pool.alloc(4);
        let mut a = AddressSpace::new(&mut alloc).unwrap();
        assert_eq!(
            a.map(&mut alloc, 0x1000, 0x1001, 0x1000, Perm::ReadWrite),
            Err(Error::Unaligned)
        );
        a.map(&mut alloc, 0x20_0000, 0x20_0000, 0x20_0000, Perm::ReadWrite)
            .unwrap();
        assert_eq!(
            a.map(&mut alloc, 0x20_1000, 0x1000, 0x1000, Perm::ReadWrite),
            Err(Error::AlreadyMapped)
        );
        assert_eq!(
            a.map(&mut alloc, 0x8000_0000, 0x1000, 0x1000, Perm::ReadWrite),
            Err(Error::OutOfTables)
        );
    }

    #[test]
    fn gigabyte_blocks() {
        let mut pool = Pool::new(4);
        let (mut alloc, access) = pool.alloc(4);
        let mut a = AddressSpace::new(&mut alloc).unwrap();
        a.map(
            &mut alloc,
            phys_to_virt(0x4000_0000),
            0x4000_0000,
            2 << 30,
            Perm::ReadWrite,
        )
        .unwrap();
        // L0 and L1 only.
        assert_eq!(alloc.remaining(), 2);
        let (pa, _) = look(&a, access, phys_to_virt(0x8765_4321)).unwrap();
        assert_eq!(pa, 0x8765_4321);
    }

    #[test]
    fn without_gigabyte_blocks() {
        let mut pool = Pool::new(8);
        let (alloc, access) = pool.alloc(8);
        let mut alloc = alloc.without_gib_blocks();
        let mut a = AddressSpace::new(&mut alloc).unwrap();
        let va = phys_to_virt(0x4000_0000);
        a.map(&mut alloc, va, 0x4000_0000, 2 << 30, Perm::ReadWrite)
            .unwrap();
        // L0, L1 and an L2 table per gigabyte.
        assert_eq!(alloc.remaining(), 4);
        // SAFETY: the tables live in `pool`.
        let e = unsafe { a.entry_at(access, va + (3 << 20), 2) }.unwrap();
        // SAFETY: as above.
        let d = unsafe { e.read_volatile() };
        assert!(is_block(d) && writable(d));
        let ro = with_read_only(d, true);
        assert!(!writable(ro) && !executable(ro) && with_read_only(ro, false) == d);
        assert!(unsafe { a.entry_at(access, va, 3) }.is_none());
    }

    #[test]
    fn pages_made_on_demand() {
        let mut pool = Pool::new(8);
        let (mut alloc, access) = pool.alloc(8);
        let a = AddressSpace::new(&mut alloc).unwrap();
        let va = 0xffff_8000_0000_0000 + 0x5000;
        let mut tables = (0..3).map(|_| alloc.alloc().unwrap()).collect::<Vec<_>>();
        let mut new = || tables.pop();
        // SAFETY: the tables live in `pool`.
        let e = unsafe { a.page_entry_with(access, va, &mut new) }.unwrap();
        // SAFETY: as above.
        unsafe { e.write_volatile(page_descriptor(0x4123_4000, Perm::Text)) };
        let (pa, d) = look(&a, access, va + 0x12).unwrap();
        assert_eq!(pa, 0x4123_4012);
        assert!(executable(d) && !writable(d));
        // The tables exist now: no more are needed for the next page.
        let mut none = || None;
        // SAFETY: as above.
        assert!(unsafe { a.page_entry_with(access, va + 0x1000, &mut none) }.is_ok());
        assert_eq!(
            unsafe { a.page_entry_with(access, va + (2 << 20), &mut none) },
            Err(Error::OutOfTables)
        );
    }
}
