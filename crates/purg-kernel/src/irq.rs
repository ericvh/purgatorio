//! Interrupts and kernel threads: the GIC and the virtual timer from the
//! DTB, a dispatch table by INTID, a 100 Hz tick that preempts, and the K1
//! acceptance test, two kernel threads that can only make progress when the
//! timer switches between them.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use purg_aarch64::dtb::{BootInfo, GicVersion};
use purg_aarch64::exception::{self, TrapFrame, Vector};
use purg_aarch64::gic::Gic;
use purg_aarch64::mmu;
use purg_aarch64::sched::Scheduler;
use purg_aarch64::timer;
use purg_platform::sync::Sched;
use spin::Once;

use crate::kernel::fail;

/// Timer interrupts per second.
pub const TICK_HZ: u64 = 100;

/// Stack for each kernel thread.
const STACK_SIZE: usize = 64 << 10;

pub static SCHED: Scheduler = Scheduler::new(STACK_SIZE);

static GIC: Once<Gic> = Once::new();
static TICKS: AtomicU64 = AtomicU64::new(0);

/// What the dispatcher does once a handler has returned and its interrupt
/// is EOId.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum After {
    Return,
    /// Give the CPU to the next thread (the timer tick).
    Preempt,
}

/// An interrupt handler. It runs with IRQs masked, before the EOI, so it
/// must quiet a level-triggered source (re-arm the timer, drain the UART)
/// and must not take `purg_platform::sync` locks or sleep.
pub type Handler = fn() -> After;

const SLOTS: usize = 8;

/// INTID and handler (a fn pointer as `usize`, 0 for a free slot) pairs.
static HANDLERS: [(AtomicU32, AtomicUsize); SLOTS] =
    [const { (AtomicU32::new(0), AtomicUsize::new(0)) }; SLOTS];

/// Routes `intid` to `h` and unmasks it at the GIC. Call after [`init`].
pub fn register(intid: u32, h: Handler) {
    let Some(gic) = GIC.get() else {
        fail(format_args!("irq: register before init"));
    };
    let Some((id, f)) = HANDLERS
        .iter()
        .find(|(_, f)| f.load(Ordering::Acquire) == 0)
    else {
        fail(format_args!("irq: no free slot for INTID {intid}"));
    };
    id.store(intid, Ordering::Relaxed);
    f.store(h as usize, Ordering::Release);
    gic.enable(intid);
}

fn handler(intid: u32) -> Option<Handler> {
    HANDLERS.iter().find_map(|(id, f)| {
        let p = f.load(Ordering::Acquire);
        (p != 0 && id.load(Ordering::Relaxed) == intid).then(|| {
            // SAFETY: only `register` stores a non-zero value, and it
            // stores a `Handler`.
            unsafe { core::mem::transmute::<usize, Handler>(p) }
        })
    })
}

fn tick_ns() -> u64 {
    1_000_000_000 / TICK_HZ
}

fn timer_tick() -> After {
    timer::arm_after_ns(tick_ns());
    TICKS.fetch_add(1, Ordering::Relaxed);
    After::Preempt
}

/// The IRQ handler: acknowledge, run the INTID's handler, EOI, and only
/// then let the scheduler switch threads, so the GIC is not left with an
/// active interrupt while another thread runs. Every interrupt counts as
/// an event for the idle logic: a thread blocked on what it changed must
/// get to look.
fn irq(_frame: &mut TrapFrame) {
    let Some(gic) = GIC.get() else { return };
    let Some(id) = gic.ack() else { return };
    let after = handler(id).map_or(After::Return, |h| h());
    gic.eoi(id);
    SCHED.wakeup();
    if after == After::Preempt {
        SCHED.preempt();
    }
}

fn sync(frame: &mut TrapFrame, esr: u64, far: u64) {
    fail(format_args!(
        "synchronous exception esr={esr:#x} elr={:#x} far={far:#x}",
        frame.elr
    ))
}

fn fatal(v: Vector, frame: &mut TrapFrame, esr: u64, far: u64) -> ! {
    fail(format_args!(
        "exception {:?}/{:?} esr={esr:#x} elr={:#x} far={far:#x}",
        v.source, v.class, frame.elr
    ))
}

/// Installs the full exception vectors, the GIC and a `TICK_HZ` timer, and
/// makes the boot thread thread 0. Returns with IRQs enabled.
pub fn init(info: &BootInfo<'_>) {
    SCHED.init();
    exception::set_irq_handler(irq);
    exception::set_sync_handler(sync);
    exception::set_fatal_handler(fatal);
    // SAFETY: FP/SIMD is enabled (boot sets CPACR_EL1.FPEN), and the
    // handlers above are registered before any exception can be taken.
    unsafe { exception::install() };

    let va = |base: u64| mmu::phys_to_virt(base as usize);
    let g = &info.gic;
    // SAFETY: the DTB names these as the GIC's frames, in the device window
    // that the high half maps as Device memory.
    let gic = unsafe {
        match g.version {
            GicVersion::V2 => Gic::v2(va(g.dist.base), va(g.cpu_or_redist.base)),
            GicVersion::V3 => Gic::v3(va(g.dist.base), va(g.cpu_or_redist.base)),
        }
    };
    let gic = GIC.call_once(|| gic);
    gic.init();
    register(info.timer_virt_intid, timer_tick);
    timer::arm_after_ns(tick_ns());
    exception::irq_enable();
}

/// Timer interrupts taken since [`init`].
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Waits, yielding, until `n` more ticks have been taken.
pub fn wait_ticks(n: u64) {
    let end = ticks() + n;
    let deadline = timer::counter() + timer::ns_to_ticks(tick_ns() * (n + 100));
    while ticks() < end {
        if timer::counter() > deadline {
            fail(format_args!("the timer stopped ticking"));
        }
        core::hint::spin_loop();
    }
}

/// Turns each thread must take.
const TURNS: u32 = 5;
/// Give up after this many ticks.
const TIMEOUT_TICKS: u64 = 10 * TICK_HZ;

/// Which thread took the last turn: 1 for A, 2 for B.
static LAST: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);

/// A thread takes a turn only when the other one took the previous turn.
/// It never yields, so without preemption the first thread to run would
/// spin forever after its first turn.
fn worker(me: u32) {
    let mut turns = 0;
    while turns < TURNS {
        if LAST.load(Ordering::Acquire) != me {
            LAST.store(me, Ordering::Release);
            turns += 1;
        }
        core::hint::spin_loop();
    }
    DONE.fetch_add(1, Ordering::AcqRel);
}

/// The K1 acceptance test: two kernel threads alternate, driven only by the
/// timer.
pub fn alternation_test() {
    let switches = SCHED.switches();
    let start = ticks();
    SCHED.spawn(Box::new(|| worker(1)));
    SCHED.spawn(Box::new(|| worker(2)));
    while DONE.load(Ordering::Acquire) < 2 {
        if ticks() - start > TIMEOUT_TICKS {
            fail(format_args!(
                "threads did not alternate: last {} after {} ticks",
                LAST.load(Ordering::Acquire),
                ticks() - start
            ));
        }
        SCHED.yield_now();
    }
    let used = SCHED.switches() - switches;
    if used < u64::from(2 * TURNS - 1) {
        fail(format_args!("only {used} switches for {TURNS} turns each"));
    }
}
