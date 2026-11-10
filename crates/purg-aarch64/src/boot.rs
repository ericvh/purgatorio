//! The boot path shared by `purg-kernel` and `purg-probe`, from `_start` at
//! the physical load address to the image's `purg_main` in the high half.
//!
//! # Contract with the image
//!
//! The image links with this crate's `link.x` (its build script passes
//! `-T crates/purg-aarch64/link.x`), which defines the section symbols
//! used here, and defines two functions:
//!
//! - `#[unsafe(no_mangle)] extern "C" fn purg_main(dtb: usize) -> !`, called
//!   in the high half with the MMU on, IRQs masked and the boot stack in
//!   place. `dtb` is x0 at entry: the DTB's physical address, or 0 when
//!   QEMU booted an ELF and left the DTB at the start of RAM ([`find_dtb`]
//!   handles both).
//! - `#[unsafe(no_mangle)] extern "C" fn purg_early_exception(kind: u64,
//!   esr: u64, elr: u64, far: u64) -> !`, called on the exception stack for
//!   any exception taken before [`crate::exception::install`].
//!
//! # `_start`
//!
//! Position-independent (only `adr`/`adrp` until the MMU is on):
//! 1. Parks every core but core 0.
//! 2. Keeps x0 (the DTB pointer, or 0) in x19 and the boot exception level
//!    in x20.
//! 3. At EL2: sets HCR_EL2.RW (EL1 is AArch64), gives EL1 the physical
//!    counter and timer (CNTHCTL_EL2.EL1PCTEN/EL1PCEN), zeroes CNTVOFF_EL2,
//!    stops EL2 trapping FP/SIMD (CPTR_EL2) and system registers (HSTR_EL2),
//!    copies MIDR/MPIDR into VPIDR/VMPIDR, and, if ID_AA64PFR0_EL1.GIC
//!    reports a system-register GIC CPU interface, sets ICC_SRE_EL2.SRE and
//!    .Enable so EL1 can use it. Then it returns to EL1h with interrupts
//!    masked.
//! 4. At EL1: resets SCTLR_EL1, enables FP/SIMD (CPACR_EL1.FPEN), sets the
//!    stack to the physical address of `__stack_top`, and zeroes .bss.
//! 5. Calls `boot_map` (Rust, MMU still off) to build the identity map and
//!    the high half in `BOOT_TABLES` from [`board`], then `boot_enable` to
//!    turn the MMU on. Execution continues at the physical address through
//!    TTBR0.
//! 6. Jumps to the high-half address of `.Lhigh`, moves the stack to its
//!    high-half address, installs the early vectors, records the boot EL
//!    and calls `purg_main(dtb)`.
//!
//! The Rust code run with the MMU off is safe there because the target is
//! `+strict-align` (no unaligned accesses to what is then Device memory),
//! the relocation model is static (symbol addresses are `adrp`/`add`, so
//! PC-relative), and [`crate::mmu`]'s builder neither indexes slices nor
//! keeps pointers in statics. Addresses read from linker symbols are
//! normalized to physical, so it would also be correct if they were
//! absolute.
//!
//! Memory written with the MMU off (non-cacheable: .bss, the tables, the
//! stack) is invalidated from the data cache (`dc ivac`) before the MMU
//! turns on, so stale lines cannot shadow it under KVM or on hardware.
//! TCG does not model caches.
//!
//! The image is also an arm64 Linux `Image` (it starts with the header),
//! so QEMU can load it Linux-style, pass the DTB in x0 and load an initrd.

use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::board::{board, dtb_size};
use crate::mmu::{
    self, KERNEL_VBASE, KernelImage, KernelMap, PAGE_SIZE, Region, SCTLR_OFF, TableAlloc,
};
use crate::pl011::Pl011;
use crate::semihost;

/// The exception level `_start` was entered at; written by the boot code.
#[unsafe(no_mangle)]
static PURG_BOOT_EL: AtomicUsize = AtomicUsize::new(0);

const BOOT_TABLE_PAGES: usize = 16;

#[repr(C, align(4096))]
struct Tables([[u64; 512]; BOOT_TABLE_PAGES]);

/// The boot page tables. The map of a 10 MiB image, the device window and
/// the RAM window takes 8 pages; each further 2 MiB of image takes one
/// more. Enough for the image, a device window and tens of
/// GiB of RAM in 1 GiB blocks.
static mut BOOT_TABLES: Tables = Tables([[0; 512]; BOOT_TABLE_PAGES]);

#[repr(C, align(16))]
struct ExceptionStack([u8; 16 * 1024]);

/// The stack the early exception path switches to, so a boot stack
/// overflow into the guard page is still reported.
#[unsafe(no_mangle)]
static mut PURG_EXCEPTION_STACK: ExceptionStack = ExceptionStack([0; 16 * 1024]);

unsafe extern "C" {
    static __image_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
    static __data_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static __image_end: u8;
}

/// The exception level the image was entered at (1 or 2).
pub fn boot_el() -> usize {
    PURG_BOOT_EL.load(Ordering::Acquire)
}

/// `a` as a physical address, whether the linker symbol was read before
/// the MMU was on (physical, PC-relative) or after (high half).
fn phys(a: *const u8) -> usize {
    let a = a as usize;
    if a >= KERNEL_VBASE {
        a - KERNEL_VBASE
    } else {
        a
    }
}

fn region(start: *const u8, end: *const u8) -> Region {
    let base = phys(start);
    Region {
        base,
        size: phys(end) - base,
    }
}

/// The physical layout of the running image, from the linker script.
pub fn image() -> KernelImage {
    let (s, t, r, d, sb, st) = (
        &raw const __image_start,
        &raw const __text_end,
        &raw const __rodata_end,
        &raw const __data_end,
        &raw const __stack_bottom,
        &raw const __stack_top,
    );
    KernelImage {
        text: region(s, t),
        rodata: region(t, r),
        data: region(r, d),
        stack: region(sb, st),
    }
}

/// The physical end of the image (page-aligned): free RAM starts here.
pub fn image_end() -> usize {
    phys(&raw const __image_end)
}

/// Builds the image's map for the board from `alloc`: the identity map,
/// and the high half with per-section permissions. This is the boot map,
/// built before the DTB can be read, so it maps [`board`]'s fixed RAM
/// window whatever RAM the machine has; nothing touches the part that is
/// not there.
pub fn kernel_map(alloc: &mut TableAlloc) -> Result<KernelMap, mmu::Error> {
    kernel_map_for(alloc, board().ram)
}

/// [`kernel_map`] over `ram`, the RAM the DTB reports.
pub fn kernel_map_for(alloc: &mut TableAlloc, ram: Region) -> Result<KernelMap, mmu::Error> {
    mmu::build_kernel_map(alloc, &[ram], board().devices, &image())
}

/// Runs with the MMU off: builds the boot tables, or reports the error on
/// the physical UART and exits.
#[unsafe(no_mangle)]
extern "C" fn purg_boot_map() -> KernelMap {
    let pool = Region {
        base: phys((&raw const BOOT_TABLES).cast()),
        size: BOOT_TABLE_PAGES * PAGE_SIZE,
    };
    // SAFETY: BOOT_TABLES is reserved for this and the MMU is off, so the
    // physical address is the access address.
    let mut alloc = unsafe { TableAlloc::new(pool, 0) };
    match kernel_map(&mut alloc) {
        Ok(map) => {
            let img = image();
            // SAFETY: with the MMU off nothing is cached, and the loader's
            // writes reached memory, so no line in .data, .bss (the tables)
            // or the stack is dirty. Discarding stale lines means the
            // cacheable accesses after the switch, the table walks and
            // the stack frames popped in `purg_boot_enable`, see what was
            // written with the MMU off.
            unsafe {
                mmu::invalidate_dcache(Region {
                    base: img.data.base,
                    size: img.stack.end() - img.data.base,
                })
            };
            map
        }
        Err(e) => {
            // SAFETY: with the MMU off the PL011 is at its physical address.
            let uart = unsafe { Pl011::new(board().uart) };
            uart.write(b"boot page tables: ");
            uart.write(e.as_str().as_bytes());
            uart.write(b"\n");
            semihost::exit(1)
        }
    }
}

/// Runs with the MMU off, from the identity-mapped physical address.
#[unsafe(no_mangle)]
extern "C" fn purg_boot_enable(ttbr0: usize, ttbr1: usize) {
    // SAFETY: `_start` calls this at EL1 with the MMU off, from RAM that
    // the boot tables identity-map; BOOT_TABLES is never freed.
    unsafe { mmu::enable(&KernelMap { ttbr0, ttbr1 }) }
}

/// Why [`find_dtb`] found no usable DTB.
#[derive(Clone, Copy, Debug)]
pub enum DtbError {
    /// No FDT magic at this physical address.
    NotFound(usize),
    /// The blob at this physical address runs outside RAM.
    OutsideRam(usize),
}

/// Finds the DTB: QEMU passes its address in x0 for a Linux-style boot and
/// 0 for a bare-metal ELF, when it is at the start of RAM. Returns the
/// blob, through the linear map, and whether it came from x0.
pub fn find_dtb(x0: usize) -> Result<(&'static [u8], bool), DtbError> {
    let ram = board().ram;
    let (pa, from_x0) = if x0 != 0 {
        (x0, true)
    } else {
        (ram.base, false)
    };
    if pa < ram.base || pa + 40 > ram.end() {
        return Err(DtbError::OutsideRam(pa));
    }
    let va = mmu::phys_to_virt(pa);
    // SAFETY: the linear map covers RAM, and the header's 40 bytes are in
    // it.
    let head = unsafe { core::slice::from_raw_parts(va as *const u8, 40) };
    let size = dtb_size(head).ok_or(DtbError::NotFound(pa))?;
    if pa + size > ram.end() {
        return Err(DtbError::OutsideRam(pa));
    }
    // SAFETY: checked to be inside the linear-mapped RAM. Nothing writes
    // the DTB, and the heap is placed clear of it.
    Ok((
        unsafe { core::slice::from_raw_parts(va as *const u8, size) },
        from_x0,
    ))
}

global_asm!(
    r#"
.section .text.boot, "ax"
// The arm64 Linux Image header (Documentation/arch/arm64/booting.rst), so
// QEMU and boot loaders can load the image as an Image: then they load an
// initrd and pass the DTB in x0. Loaded as an ELF, the first instruction
// simply branches over it.
.global _head
_head:
    b       _start                  // code0
    .long   0                       // code1
    .quad   0x200000                // text_offset: RAM base + 2 MiB
    .quad   __image_size            // image_size, including bss and stack
    .quad   0x2                     // flags: little-endian, 4 KiB pages
    .quad   0, 0, 0                 // res2..res4
    .ascii  "ARM\x64"               // magic
    .long   0                       // res5: no PE header

.global _start
_start:
    mrs     x1, mpidr_el1
    and     x1, x1, #0xffffff
    cbnz    x1, .Lpark

    mov     x19, x0
    mrs     x20, CurrentEL
    lsr     x20, x20, #2
    cmp     x20, #2
    b.ne    .Lel1

    mov     x1, #(1 << 31)
    msr     hcr_el2, x1
    mrs     x1, cnthctl_el2
    orr     x1, x1, #3
    msr     cnthctl_el2, x1
    msr     cntvoff_el2, xzr
    mov     x1, #0x33ff
    msr     cptr_el2, x1
    msr     hstr_el2, xzr
    mrs     x1, midr_el1
    msr     vpidr_el2, x1
    mrs     x1, mpidr_el1
    msr     vmpidr_el2, x1
    mrs     x1, id_aa64pfr0_el1
    ubfx    x1, x1, #24, #4
    cbz     x1, 1f
    mrs     x1, icc_sre_el2
    mov     x2, #(1 | (1 << 3))
    orr     x1, x1, x2
    msr     icc_sre_el2, x1
    isb
1:
    ldr     x1, ={sctlr_off}
    msr     sctlr_el1, x1
    mov     x1, #0x3c5
    msr     spsr_el2, x1
    adr     x1, .Lel1
    msr     elr_el2, x1
    eret

.Lel1:
    ldr     x1, ={sctlr_off}
    msr     sctlr_el1, x1
    mov     x1, #(3 << 20)
    msr     cpacr_el1, x1
    isb

    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    mov     sp, x1

    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
.Lzero:
    cmp     x1, x2
    b.hs    .Lzeroed
    stp     xzr, xzr, [x1], #16
    b       .Lzero
.Lzeroed:

    bl      purg_boot_map
    bl      purg_boot_enable

    ldr     x1, =.Lhigh
    br      x1
.Lhigh:
    ldr     x1, =__stack_top
    mov     sp, x1
    adrp    x1, purg_early_vectors
    add     x1, x1, :lo12:purg_early_vectors
    msr     vbar_el1, x1
    isb
    adrp    x1, PURG_BOOT_EL
    str     x20, [x1, :lo12:PURG_BOOT_EL]
    mov     x0, x19
    bl      purg_main

.Lpark:
    wfe
    b       .Lpark
.ltorg

.section .text.purg_early_vectors, "ax"
.balign 2048
.global purg_early_vectors
purg_early_vectors:
.irp kind, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    .balign 0x80
    mov     x0, #\kind
    b       .Learly_exception
.endr

.Learly_exception:
    adrp    x9, PURG_EXCEPTION_STACK
    add     x9, x9, :lo12:PURG_EXCEPTION_STACK
    add     x9, x9, #{exc_stack}
    mov     sp, x9
    mrs     x1, esr_el1
    mrs     x2, elr_el1
    mrs     x3, far_el1
    bl      purg_early_exception
1:  wfe
    b       1b
"#,
    sctlr_off = const SCTLR_OFF,
    exc_stack = const core::mem::size_of::<ExceptionStack>(),
);
