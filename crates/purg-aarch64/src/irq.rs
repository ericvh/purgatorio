//! Interrupts: the GIC and the virtual timer from the DTB, a dispatch table
//! by INTID, and a periodic tick that preempts the running thread.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use purg_platform::sync::Sched;
use spin::Once;

use crate::dtb::{BootInfo, GicVersion};
use crate::exception::{self, TrapFrame};
use crate::gic::Gic;
use crate::mmu;
use crate::sched::Scheduler;
use crate::timer;

static GIC: Once<Gic> = Once::new();
static SCHED: Once<&'static Scheduler> = Once::new();
static TICKS: AtomicU64 = AtomicU64::new(0);
static TICK_NS: AtomicU64 = AtomicU64::new(0);

/// What the dispatcher does once a handler has returned and its interrupt
/// is EOId.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum After {
    Return,
    /// Give the CPU to the next thread (the timer tick).
    Preempt,
}

/// An interrupt handler. It runs with IRQs masked, before the EOI, so it
/// must quiet a level-triggered source (re-arm the timer, drain the UART,
/// acknowledge a virtio device) and must not take `purg_platform::sync`
/// locks or sleep.
pub type Handler = fn() -> After;

const SLOTS: usize = 8;

/// INTID and handler (a fn pointer as `usize`, 0 for a free slot) pairs.
static HANDLERS: [(AtomicU32, AtomicUsize); SLOTS] =
    [const { (AtomicU32::new(0), AtomicUsize::new(0)) }; SLOTS];

/// Routes `intid` to `h` and unmasks it at the GIC. Call after [`init`].
///
/// # Panics
/// Before [`init`], or when all slots are taken.
pub fn register(intid: u32, h: Handler) {
    let Some(gic) = GIC.get() else {
        panic!("irq: register before init");
    };
    let Some((id, f)) = HANDLERS
        .iter()
        .find(|(_, f)| f.load(Ordering::Acquire) == 0)
    else {
        panic!("irq: no free slot for INTID {intid}");
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

fn timer_tick() -> After {
    timer::arm_after_ns(TICK_NS.load(Ordering::Relaxed));
    TICKS.fetch_add(1, Ordering::Relaxed);
    After::Preempt
}

/// The IRQ handler: acknowledge, run the INTID's handler, EOI, and only
/// then let the scheduler switch threads, so the GIC is not left with an
/// active interrupt while another thread runs. Every interrupt counts as
/// an event for the idle logic: a thread blocked on what it changed must
/// get to look.
fn irq(_frame: &mut TrapFrame) {
    let (Some(gic), Some(sched)) = (GIC.get(), SCHED.get()) else {
        return;
    };
    let Some(id) = gic.ack() else { return };
    let after = handler(id).map_or(After::Return, |h| h());
    gic.eoi(id);
    sched.wakeup();
    if after == After::Preempt {
        sched.preempt();
    }
}

/// Makes the boot thread thread 0 of `sched`, installs the full exception
/// vectors, the GIC from the DTB and a `tick_hz` timer that preempts, and
/// returns with IRQs enabled. Set the synchronous and fatal handlers
/// ([`exception::set_sync_handler`], [`exception::set_fatal_handler`])
/// first.
pub fn init(info: &BootInfo<'_>, sched: &'static Scheduler, tick_hz: u64) {
    sched.init();
    SCHED.call_once(|| sched);
    TICK_NS.store(1_000_000_000 / tick_hz.max(1), Ordering::Relaxed);
    exception::set_irq_handler(irq);
    // SAFETY: FP/SIMD is enabled (boot sets CPACR_EL1.FPEN), and the IRQ
    // handler is registered before an IRQ can be taken.
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
    timer::arm_after_ns(TICK_NS.load(Ordering::Relaxed));
    exception::irq_enable();
}

/// Timer interrupts taken since [`init`].
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// The tick period in nanoseconds.
pub fn tick_ns() -> u64 {
    TICK_NS.load(Ordering::Relaxed)
}
