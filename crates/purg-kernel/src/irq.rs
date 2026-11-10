//! Interrupts and kernel threads: the GIC and the virtual timer from the
//! DTB, a 100 Hz tick that preempts, and the K1 acceptance test, two kernel
//! threads that can only make progress when the timer switches between
//! them.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
/// wasmi and the host calls it makes need deep stacks, as in the probe.
const STACK_SIZE: usize = 2 << 20;

pub static SCHED: Scheduler = Scheduler::new(STACK_SIZE);

static GIC: Once<Gic> = Once::new();
static TIMER_INTID: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

fn tick_ns() -> u64 {
    1_000_000_000 / TICK_HZ
}

/// The IRQ handler. The timer is level-triggered, so it is re-armed before
/// the EOI; only then may the scheduler switch threads, so the GIC is not
/// left with an active interrupt while another thread runs.
fn irq(_frame: &mut TrapFrame) {
    let Some(gic) = GIC.get() else { return };
    let Some(id) = gic.ack() else { return };
    if id == TIMER_INTID.load(Ordering::Relaxed) {
        timer::arm_after_ns(tick_ns());
        TICKS.fetch_add(1, Ordering::Relaxed);
        gic.eoi(id);
        SCHED.preempt();
    } else {
        gic.eoi(id);
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
    TIMER_INTID.store(info.timer_virt_intid, Ordering::Relaxed);
    gic.enable(info.timer_virt_intid);
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
