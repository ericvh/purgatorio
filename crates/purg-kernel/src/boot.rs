//! Boot path, from `_start` at the physical load address to
//! [`crate::kernel_main`] in the high half.
//!
//! `_start` (assembly, position-independent: only `adr`/`adrp` until the
//! MMU is on):
//! 1. Parks every core but core 0.
//! 2. Keeps x0 (the DTB pointer, or 0 when QEMU boots an ELF) in x19 and
//!    the boot exception level in x20.
//! 3. At EL2: sets HCR_EL2.RW (EL1 is AArch64), gives EL1 the physical
//!    counter and timer (CNTHCTL_EL2.EL1PCTEN/EL1PCEN), zeroes CNTVOFF_EL2,
//!    stops EL2 trapping FP/SIMD (CPTR_EL2) and system registers (HSTR_EL2),
//!    copies MIDR/MPIDR into VPIDR/VMPIDR, and, if ID_AA64PFR0_EL1.GIC
//!    reports a system-register GIC CPU interface, sets ICC_SRE_EL2.SRE and
//!    .Enable so EL1 can use it. Then it returns to EL1h with interrupts
//!    masked.
//! 4. At EL1: resets SCTLR_EL1, enables FP/SIMD (CPACR_EL1.FPEN), sets the
//!    stack to the physical address of `__stack_top`, and zeroes .bss.
//! 5. Calls [`boot_map`] (Rust, MMU still off) to build the identity map and
//!    the high half in `BOOT_TABLES`, then [`boot_enable`] to turn the MMU
//!    on. Execution continues at the physical address through TTBR0.
//! 6. Loads the absolute (high) address of `.Lhigh` and jumps there, moves
//!    the stack to its high-half address, installs the exception vectors,
//!    records the boot EL and calls `kernel_main(dtb)`.
//!
//! The Rust code run with the MMU off is safe there because the target is
//! `+strict-align` (no unaligned accesses to what is then Device memory),
//! the relocation model is static (symbol addresses are `adrp`/`add`, so
//! PC-relative), and [`purg_aarch64::mmu`]'s builder neither indexes
//! slices nor keeps pointers in statics. Addresses it reads from linker
//! symbols are normalized to physical, so it would also be correct if they
//! were absolute.
//!
//! Memory written with the MMU off (non-cacheable: .bss, the tables, the
//! stack) is invalidated from the data cache (`dc ivac`) before the MMU
//! turns on, so stale lines cannot shadow it under KVM or on hardware.
//! TCG does not model caches.

use core::arch::global_asm;
use core::sync::atomic::AtomicUsize;

use purg_aarch64::mmu::{
    self, KERNEL_VBASE, KernelImage, KernelMap, PAGE_SIZE, Region, SCTLR_OFF, TableAlloc,
};
use purg_aarch64::pl011::Pl011;
use purg_aarch64::semihost;

use crate::board::board;

/// The exception level `_start` was entered at; written by the boot code.
#[unsafe(no_mangle)]
pub static KERNEL_BOOT_EL: AtomicUsize = AtomicUsize::new(0);

const BOOT_TABLE_PAGES: usize = 32;

#[repr(C, align(4096))]
struct Tables([[u64; 512]; BOOT_TABLE_PAGES]);

/// The boot page tables. Enough for the image, a device window and tens of
/// GiB of RAM in 1 GiB blocks.
static mut BOOT_TABLES: Tables = Tables([[0; 512]; BOOT_TABLE_PAGES]);

#[repr(C, align(16))]
struct ExceptionStack([u8; 16 * 1024]);

/// The stack the fatal-exception path switches to, so a boot stack
/// overflow into the guard page is still reported.
#[unsafe(no_mangle)]
static mut EXCEPTION_STACK: ExceptionStack = ExceptionStack([0; 16 * 1024]);

unsafe extern "C" {
    static __image_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
    static __data_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static __image_end: u8;
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

/// Builds the kernel map for the current board and image from `alloc`.
pub fn kernel_map(alloc: &mut TableAlloc) -> Result<KernelMap, mmu::Error> {
    let b = board();
    mmu::build_kernel_map(alloc, &[b.ram], b.devices, &image())
}

/// Runs with the MMU off: builds the boot tables, or reports the error on
/// the physical UART and exits.
#[unsafe(no_mangle)]
extern "C" fn boot_map() -> KernelMap {
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
            // the stack frames popped in `boot_enable`, see what was
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
            uart.write(b"kernel: boot page tables: ");
            uart.write(e.as_str().as_bytes());
            uart.write(b"\n");
            semihost::exit(1)
        }
    }
}

/// Runs with the MMU off, from the identity-mapped physical address.
#[unsafe(no_mangle)]
extern "C" fn boot_enable(ttbr0: usize, ttbr1: usize) {
    // SAFETY: `_start` calls this at EL1 with the MMU off, from RAM that
    // the boot tables identity-map; BOOT_TABLES is never freed.
    unsafe { mmu::enable(&KernelMap { ttbr0, ttbr1 }) }
}

global_asm!(
    r#"
.section .text.boot, "ax"
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

    bl      boot_map
    bl      boot_enable

    ldr     x1, =.Lhigh
    br      x1
.Lhigh:
    ldr     x1, =__stack_top
    mov     sp, x1
    adrp    x1, kernel_vectors
    add     x1, x1, :lo12:kernel_vectors
    msr     vbar_el1, x1
    isb
    adrp    x1, KERNEL_BOOT_EL
    str     x20, [x1, :lo12:KERNEL_BOOT_EL]
    mov     x0, x19
    bl      kernel_main

.Lpark:
    wfe
    b       .Lpark
.ltorg

.section .text.vectors, "ax"
.balign 2048
.global kernel_vectors
kernel_vectors:
.irp kind, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    .balign 0x80
    mov     x0, #\kind
    b       .Lexception
.endr

.Lexception:
    adrp    x9, EXCEPTION_STACK
    add     x9, x9, :lo12:EXCEPTION_STACK
    add     x9, x9, #{exc_stack}
    mov     sp, x9
    mrs     x1, esr_el1
    mrs     x2, elr_el1
    mrs     x3, far_el1
    bl      kernel_exception
    b       .Lpark
"#,
    sctlr_off = const SCTLR_OFF,
    exc_stack = const core::mem::size_of::<ExceptionStack>(),
);
