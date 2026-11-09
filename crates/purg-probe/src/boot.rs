//! Boot stub for QEMU virt: park secondary cores, drop from EL2 to EL1 if
//! needed, enable FP/SIMD, zero .bss, install exception vectors, turn on
//! the MMU with an identity map, and call [`crate::probe_main`].
//!
//! The MMU is required, not an optimisation: with it off every access is
//! Device memory, where unaligned accesses fault and exclusives are
//! unreliable.
//!
//! Identity map (one level-1 table, 4 KiB granule, 39-bit VA, 1 GiB blocks):
//! - 0x0000_0000..0x4000_0000: Device-nGnRnE, execute-never (GIC, PL011, PL031).
//! - 0x4000_0000..0x1_0000_0000: Normal write-back, inner shareable (RAM).

use core::arch::global_asm;

/// MAIR_EL1: attr0 = Device-nGnRnE, attr1 = Normal WB RA/WA.
pub const MAIR: u64 = 0xff << 8;

/// TCR_EL1: T0SZ=25, IRGN0=ORGN0=WB, SH0=inner, TG0=4K, EPD1, IPS=36 bits.
pub const TCR: u64 = 25 | (1 << 8) | (1 << 10) | (3 << 12) | (1 << 23) | (1 << 32);

const BLOCK: u64 = 0b01;
const AF: u64 = 1 << 10;
const SH_INNER: u64 = 3 << 8;
const ATTR_NORMAL: u64 = 1 << 2;
const XN: u64 = (1 << 54) | (1 << 53);

pub const DEVICE_BLOCK: u64 = BLOCK | AF | XN;
pub const NORMAL_BLOCK: u64 = BLOCK | AF | SH_INNER | ATTR_NORMAL;

#[repr(C, align(4096))]
pub struct Table(pub [u64; 512]);

#[unsafe(no_mangle)]
static mut PT_L1: Table = Table([0; 512]);

global_asm!(
    r#"
.section .text.boot, "ax"
.global _start
_start:
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbnz    x0, .Lpark

    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    .Lel1
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, .Lel1
    msr     elr_el2, x0
    eret

.Lel1:
    ldr     x0, =__stack_top
    mov     sp, x0

    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb

    ldr     x0, =__bss_start
    ldr     x1, =__bss_end
.Lzero:
    cmp     x0, x1
    b.hs    .Lzeroed
    stp     xzr, xzr, [x0], #16
    b       .Lzero
.Lzeroed:

    ldr     x0, =vectors
    msr     vbar_el1, x0

    ldr     x0, =PT_L1
    ldr     x1, ={device}
    str     x1, [x0]
    ldr     x1, ={normal}
    mov     x2, #0x40000000
    orr     x3, x1, x2
    str     x3, [x0, #8]
    add     x3, x3, x2
    str     x3, [x0, #16]
    add     x3, x3, x2
    str     x3, [x0, #24]
    dsb     ishst

    msr     ttbr0_el1, x0
    ldr     x1, ={mair}
    msr     mair_el1, x1
    ldr     x1, ={tcr}
    msr     tcr_el1, x1
    isb
    tlbi    vmalle1
    dsb     ish
    isb

    mrs     x0, sctlr_el1
    orr     x0, x0, #(1 << 0)
    orr     x0, x0, #(1 << 2)
    orr     x0, x0, #(1 << 12)
    bic     x0, x0, #(1 << 1)
    msr     sctlr_el1, x0
    isb

    bl      probe_main

.Lpark:
    wfe
    b       .Lpark

.section .text.vectors, "ax"
.balign 2048
.global vectors
vectors:
.irp kind, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    .balign 0x80
    mov     x0, #\kind
    b       .Lexception
.endr

.Lexception:
    mrs     x1, esr_el1
    mrs     x2, elr_el1
    mrs     x3, far_el1
    bl      probe_exception
    b       .Lpark
"#,
    device = const DEVICE_BLOCK,
    normal = const NORMAL_BLOCK,
    mair = const MAIR,
    tcr = const TCR,
);
