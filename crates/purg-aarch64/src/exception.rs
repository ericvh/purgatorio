//! EL1 exception vectors with a full trap frame, IRQ dispatch, and IRQ
//! masking helpers.
//!
//! Kernel threads run at EL1 on `SP_EL1` (EL1h). Exceptions from that state
//! ("current EL with SP_ELx") save a [`TrapFrame`] on the interrupted
//! thread's own stack, call the registered handler with `&mut TrapFrame`,
//! restore everything from the frame and `eret`. Every other vector (current
//! EL with SP_EL0, lower EL, FIQ, SError) goes to the fatal handler.
//!
//! # Switching threads inside the IRQ handler
//!
//! The IRQ handler may switch to another kernel thread with a callee-saved
//! context switch and return much later, on the same thread it interrupted.
//! This works because:
//!
//! - the frame is on the interrupted thread's stack, not in a per-CPU save
//!   area, and the entry code keeps nothing else live across the call;
//! - `ELR_EL1` and `SPSR_EL1` are part of the frame, so other exceptions
//!   taken while this thread is switched out cannot corrupt them;
//! - the exit path masks all of DAIF before it writes `ELR_EL1`/`SPSR_EL1`,
//!   whatever DAIF the handler (or the thread that switched back to us)
//!   left; the interrupted context's own DAIF comes back from `SPSR` on
//!   `eret`.
//!
//! Before switching, the handler must have acknowledged and EOId its
//! interrupt at the GIC and re-armed or disabled the timer (it is
//! level-triggered), or no further interrupt of that priority is delivered.
//! The stack stays 16-byte aligned throughout: the frame size is a multiple
//! of 16.

use core::arch::{asm, global_asm};
use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Registers saved on exception entry, on the current stack.
///
/// Layout (byte offsets, `#[repr(C)]`, 800 bytes, 16-byte aligned):
///
/// | offset | field  | contents                                         |
/// |-------:|--------|--------------------------------------------------|
/// |      0 | `x`    | x0..x30                                          |
/// |    248 | `sp`   | SP_EL1 before the exception (informational)      |
/// |    256 | `elr`  | ELR_EL1: where `eret` resumes                    |
/// |    264 | `spsr` | SPSR_EL1: PSTATE (incl. DAIF) restored on `eret`  |
/// |    272 | `fpcr` | FPCR                                             |
/// |    280 | `fpsr` | FPSR                                             |
/// |    288 | `q`    | q0..q31, full 128 bits                           |
///
/// Every field except `sp` is written back on return, so a handler can
/// change the resumed state, for example `elr += 4` to skip a `brk`.
/// `sp` is not restored: the frame sits just below it.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp: u64,
    pub elr: u64,
    pub spsr: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub q: [u128; 32],
}

/// Size of [`TrapFrame`] in bytes; the entry code depends on it.
pub const FRAME_SIZE: usize = 800;
const _: () = assert!(core::mem::size_of::<TrapFrame>() == FRAME_SIZE);
const _: () = assert!(FRAME_SIZE.is_multiple_of(16));
const _: () = assert!(core::mem::offset_of!(TrapFrame, sp) == 248);
const _: () = assert!(core::mem::offset_of!(TrapFrame, elr) == 256);
const _: () = assert!(core::mem::offset_of!(TrapFrame, fpcr) == 272);
const _: () = assert!(core::mem::offset_of!(TrapFrame, q) == 288);

/// Where an exception came from: the four groups of the vector table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Current EL, SP_EL0.
    CurrentSp0,
    /// Current EL, SP_ELx: kernel threads.
    CurrentSpx,
    /// Lower EL, AArch64.
    Lower64,
    /// Lower EL, AArch32.
    Lower32,
}

/// The exception type within a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Sync,
    Irq,
    Fiq,
    SError,
}

/// One of the 16 vector table entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vector {
    pub source: Source,
    pub class: Class,
}

impl Vector {
    /// The entry at `index` (0..16) of the table.
    pub fn from_index(index: u64) -> Vector {
        let source = match (index >> 2) & 3 {
            0 => Source::CurrentSp0,
            1 => Source::CurrentSpx,
            2 => Source::Lower64,
            _ => Source::Lower32,
        };
        let class = match index & 3 {
            0 => Class::Sync,
            1 => Class::Irq,
            2 => Class::Fiq,
            _ => Class::SError,
        };
        Vector { source, class }
    }
}

/// The handler for exceptions the kernel cannot handle. It must not return.
pub type FatalHandler = fn(Vector, &mut TrapFrame, u64, u64) -> !;

/// Fn pointers stored as `usize`; 0 means "use the default".
static IRQ_HANDLER: AtomicUsize = AtomicUsize::new(0);
static SYNC_HANDLER: AtomicUsize = AtomicUsize::new(0);
static FATAL_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Sets the handler for IRQs taken at EL1h. Until one is set, an IRQ is
/// fatal.
pub fn set_irq_handler(h: fn(&mut TrapFrame)) {
    IRQ_HANDLER.store(h as usize, Ordering::Release);
}

/// Sets the handler for synchronous exceptions taken at EL1h, called with
/// `ESR_EL1` and `FAR_EL1`. Until one is set, they are fatal.
pub fn set_sync_handler(h: fn(&mut TrapFrame, u64, u64)) {
    SYNC_HANDLER.store(h as usize, Ordering::Release);
}

/// Sets the fatal handler, called for every vector other than EL1h sync
/// and IRQ, and for those two when no handler is set. The default exits
/// QEMU through semihosting with `0x80 | vector index` and parks the core
/// if semihosting is off.
pub fn set_fatal_handler(h: FatalHandler) {
    FATAL_HANDLER.store(h as usize, Ordering::Release);
}

fn default_fatal(v: Vector, _frame: &mut TrapFrame, _esr: u64, _far: u64) -> ! {
    let group = match v.source {
        Source::CurrentSp0 => 0,
        Source::CurrentSpx => 1,
        Source::Lower64 => 2,
        Source::Lower32 => 3,
    };
    let class = match v.class {
        Class::Sync => 0,
        Class::Irq => 1,
        Class::Fiq => 2,
        Class::SError => 3,
    };
    crate::semihost::exit(0x80 | (group << 2) | class)
}

/// Calls the fatal handler for `v`.
pub fn fatal(v: Vector, frame: &mut TrapFrame, esr: u64, far: u64) -> ! {
    let p = FATAL_HANDLER.load(Ordering::Acquire);
    if p == 0 {
        default_fatal(v, frame, esr, far)
    } else {
        // SAFETY: only `set_fatal_handler` stores a non-zero value, and it
        // stores a `FatalHandler`.
        let h: FatalHandler = unsafe { core::mem::transmute::<usize, FatalHandler>(p) };
        h(v, frame, esr, far)
    }
}

/// Called by the vector code with the frame it built on the current stack.
#[unsafe(no_mangle)]
extern "C" fn purg_aarch64_trap(frame: &mut TrapFrame, index: u64, esr: u64, far: u64) {
    let v = Vector::from_index(index);
    match (v.source, v.class) {
        (Source::CurrentSpx, Class::Irq) => {
            let p = IRQ_HANDLER.load(Ordering::Acquire);
            if p == 0 {
                fatal(v, frame, esr, far);
            }
            // SAFETY: only `set_irq_handler` stores a non-zero value.
            let h = unsafe { core::mem::transmute::<usize, fn(&mut TrapFrame)>(p) };
            h(frame);
        }
        (Source::CurrentSpx, Class::Sync) => {
            let p = SYNC_HANDLER.load(Ordering::Acquire);
            if p == 0 {
                fatal(v, frame, esr, far);
            }
            // SAFETY: only `set_sync_handler` stores a non-zero value.
            let h = unsafe { core::mem::transmute::<usize, fn(&mut TrapFrame, u64, u64)>(p) };
            h(frame, esr, far);
        }
        _ => fatal(v, frame, esr, far),
    }
}

// Each vector entry reserves the frame, saves x0/x1 and jumps to the common
// code with the entry's index in x0. The common code saves the rest, calls
// `purg_aarch64_trap(frame, index, esr, far)`, and restores from the frame.
global_asm!(
    r#"
.section .text.purg_aarch64_vectors, "ax"
.balign 2048
.global purg_aarch64_vectors
purg_aarch64_vectors:
.irp index, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    .balign 0x80
    sub     sp, sp, #{frame}
    stp     x0, x1, [sp, #0]
    mov     x0, #\index
    b       purg_aarch64_trap_entry
.endr

.balign 16
purg_aarch64_trap_entry:
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    add     x1, sp, #{frame}
    stp     x30, x1, [sp, #240]
    mrs     x1, elr_el1
    mrs     x2, spsr_el1
    stp     x1, x2, [sp, #256]
    mrs     x1, fpcr
    mrs     x2, fpsr
    stp     x1, x2, [sp, #272]
    add     x1, sp, #288
    stp     q0, q1, [x1, #0]
    stp     q2, q3, [x1, #32]
    stp     q4, q5, [x1, #64]
    stp     q6, q7, [x1, #96]
    stp     q8, q9, [x1, #128]
    stp     q10, q11, [x1, #160]
    stp     q12, q13, [x1, #192]
    stp     q14, q15, [x1, #224]
    stp     q16, q17, [x1, #256]
    stp     q18, q19, [x1, #288]
    stp     q20, q21, [x1, #320]
    stp     q22, q23, [x1, #352]
    stp     q24, q25, [x1, #384]
    stp     q26, q27, [x1, #416]
    stp     q28, q29, [x1, #448]
    stp     q30, q31, [x1, #480]

    // A frame record linking to the interrupted code, for backtraces.
    ldr     x2, [sp, #256]
    stp     x29, x2, [sp, #-16]!
    mov     x29, sp

    mov     x1, x0
    add     x0, sp, #16
    mrs     x2, esr_el1
    mrs     x3, far_el1
    bl      purg_aarch64_trap

    add     sp, sp, #16
    msr     daifset, #0xf
    add     x1, sp, #288
    ldp     q0, q1, [x1, #0]
    ldp     q2, q3, [x1, #32]
    ldp     q4, q5, [x1, #64]
    ldp     q6, q7, [x1, #96]
    ldp     q8, q9, [x1, #128]
    ldp     q10, q11, [x1, #160]
    ldp     q12, q13, [x1, #192]
    ldp     q14, q15, [x1, #224]
    ldp     q16, q17, [x1, #256]
    ldp     q18, q19, [x1, #288]
    ldp     q20, q21, [x1, #320]
    ldp     q22, q23, [x1, #352]
    ldp     q24, q25, [x1, #384]
    ldp     q26, q27, [x1, #416]
    ldp     q28, q29, [x1, #448]
    ldp     q30, q31, [x1, #480]
    ldp     x1, x2, [sp, #272]
    msr     fpcr, x1
    msr     fpsr, x2
    ldp     x1, x2, [sp, #256]
    msr     elr_el1, x1
    msr     spsr_el1, x2
    ldp     x0, x1, [sp, #0]
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    add     sp, sp, #{frame}
    eret
"#,
    frame = const FRAME_SIZE,
);

unsafe extern "C" {
    static purg_aarch64_vectors: u8;
}

/// Points `VBAR_EL1` at the vector table.
///
/// # Safety
/// Must run at EL1 with FP/SIMD enabled (`CPACR_EL1.FPEN`), since the entry
/// code saves the q registers. Replaces whatever vectors were installed.
pub unsafe fn install() {
    let base = &raw const purg_aarch64_vectors as u64;
    // SAFETY: the table is 2 KiB aligned and lives for the whole run.
    unsafe { asm!("msr vbar_el1, {}", "isb", in(reg) base, options(nostack)) };
}

const DAIF_I: u64 = 1 << 7;

/// Unmasks IRQs.
#[inline]
pub fn irq_enable() {
    // SAFETY: changes only PSTATE.I. Not `nomem`: it orders memory accesses.
    unsafe { asm!("msr daifclr, #2", options(nostack, preserves_flags)) };
}

/// Masks IRQs.
#[inline]
pub fn irq_disable() {
    // SAFETY: as in `irq_enable`.
    unsafe { asm!("msr daifset, #2", options(nostack, preserves_flags)) };
}

/// Masks IRQs and returns the previous DAIF, for [`irq_restore`].
#[inline]
pub fn irq_save() -> u64 {
    let daif: u64;
    // SAFETY: as in `irq_enable`.
    unsafe {
        asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nostack, preserves_flags))
    };
    daif
}

/// Restores DAIF saved by [`irq_save`].
#[inline]
pub fn irq_restore(daif: u64) {
    // SAFETY: as in `irq_enable`.
    unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack, preserves_flags)) };
}

/// Whether IRQs are unmasked.
#[inline]
pub fn irqs_enabled() -> bool {
    let daif: u64;
    // SAFETY: reads PSTATE.DAIF only.
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    daif & DAIF_I == 0
}

/// Masks IRQs while it lives and restores the previous state on drop.
/// Not `Send`: DAIF belongs to the CPU it was saved on.
pub struct IrqGuard {
    daif: u64,
    _not_send: PhantomData<*const ()>,
}

impl IrqGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> IrqGuard {
        IrqGuard {
            daif: irq_save(),
            _not_send: PhantomData,
        }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        irq_restore(self.daif);
    }
}
