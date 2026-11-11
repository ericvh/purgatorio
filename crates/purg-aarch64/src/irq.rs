//! Interrupts: the GIC and the virtual timer from the DTB, a dispatch table
//! by INTID, a periodic tick on every CPU that preempts the running
//! thread, and the reschedule IPI (SGI [`IPI_INTID`]) the scheduler sends
//! to wake a waiting CPU.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use purg_platform::sync::Sched;
use spin::Once;

use crate::cpu::{self, MAX_CPUS};
use crate::dtb::{BootInfo, GicVersion};
use crate::exception::{self, TrapFrame};
use crate::gic::Gic;
use crate::mmu;
use crate::sched::Scheduler;
use crate::timer;

/// The SGI that interrupts a waiting CPU. Its handler does nothing: the
/// CPU leaves `wfi` and looks again.
pub const IPI_INTID: u32 = 1;

static GIC: Once<Gic> = Once::new();
static SCHED: Once<&'static Scheduler> = Once::new();
/// Ticks of CPU 0's timer: the kernel's tick clock.
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Timer interrupts by CPU.
static CPU_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// IPIs taken by CPU.
static CPU_IPIS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static TICK_NS: AtomicU64 = AtomicU64::new(0);
/// The timer's PPI.
static TIMER_INTID: AtomicU32 = AtomicU32::new(u32::MAX);

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
/// locks or sleep. Peripheral interrupts are taken on CPU 0; the timer's
/// and the IPI on every CPU.
pub type Handler = fn() -> After;

const SLOTS: usize = 8;

/// INTID and handler (a fn pointer as `usize`, 0 for a free slot) pairs.
/// A slot is claimed by compare-and-swap, so CPUs may register at once.
static HANDLERS: [(AtomicU32, AtomicUsize); SLOTS] =
    [const { (AtomicU32::new(0), AtomicUsize::new(0)) }; SLOTS];

/// A claimed slot whose handler is not stored yet.
const CLAIMED: usize = 1;

/// Routes `intid` to `h` and unmasks it at the GIC. An SGI or PPI is
/// unmasked on the calling CPU, and on each secondary CPU as it comes up
/// ([`init_cpu`]); register those before [`crate::smp`] starts the
/// secondaries. Call after [`init`].
///
/// # Panics
/// Before [`init`], or when all slots are taken.
pub fn register(intid: u32, h: Handler) {
    let Some(gic) = GIC.get() else {
        panic!("irq: register before init");
    };
    let Some((id, f)) = HANDLERS.iter().find(|(_, f)| {
        f.compare_exchange(0, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }) else {
        panic!("irq: no free slot for INTID {intid}");
    };
    id.store(intid, Ordering::Relaxed);
    f.store(h as usize, Ordering::Release);
    let _g = exception::IrqGuard::new();
    gic.enable(intid);
}

fn handler(intid: u32) -> Option<Handler> {
    HANDLERS.iter().find_map(|(id, f)| {
        let p = f.load(Ordering::Acquire);
        (p > CLAIMED && id.load(Ordering::Relaxed) == intid).then(|| {
            // SAFETY: only `register` stores a value above CLAIMED, and it
            // stores a `Handler`.
            unsafe { core::mem::transmute::<usize, Handler>(p) }
        })
    })
}

fn timer_tick() -> After {
    timer::arm_after_ns(TICK_NS.load(Ordering::Relaxed));
    let cpu = cpu::id();
    if cpu == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    CPU_TICKS[cpu].fetch_add(1, Ordering::Relaxed);
    After::Preempt
}

fn ipi() -> After {
    CPU_IPIS[cpu::id()].fetch_add(1, Ordering::Relaxed);
    After::Return
}

/// Sends the reschedule IPI to the CPUs in `cpus`, for the scheduler.
fn send_ipi(cpus: u64) {
    if let Some(gic) = GIC.get() {
        gic.send_sgi(IPI_INTID, cpus);
    }
}

/// The IRQ handler: acknowledge, run the INTID's handler, EOI, and only
/// then let the scheduler switch threads, so the GIC is not left with an
/// active interrupt while another thread runs. Every interrupt counts as
/// an event for the idle logic: a thread blocked on what it changed must
/// get to look. A device interrupt may have changed what a thread on any
/// CPU waits for, so it wakes them all; the tick and the IPI only matter
/// to this CPU.
fn irq(_frame: &mut TrapFrame) {
    let (Some(gic), Some(sched)) = (GIC.get(), SCHED.get()) else {
        return;
    };
    let Some(a) = gic.ack() else { return };
    let after = handler(a.intid).map_or(After::Return, |h| h());
    gic.eoi(a);
    if a.intid == IPI_INTID || a.intid == TIMER_INTID.load(Ordering::Relaxed) {
        sched.local_event();
    } else {
        sched.wakeup();
    }
    if after == After::Preempt {
        if cpu::id() == 0 {
            sched.expire();
        }
        sched.preempt();
    }
}

/// Makes the boot thread thread 0 of `sched`, installs the full exception
/// vectors, the GIC from the DTB, a `tick_hz` timer that preempts and the
/// reschedule IPI, and returns with IRQs enabled. Set the synchronous and
/// fatal handlers ([`exception::set_sync_handler`],
/// [`exception::set_fatal_handler`]) first.
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
    {
        let _g = exception::IrqGuard::new();
        gic.init();
    }
    TIMER_INTID.store(info.timer_virt_intid, Ordering::Relaxed);
    register(info.timer_virt_intid, timer_tick);
    register(IPI_INTID, ipi);
    sched.set_ipi(send_ipi);
    timer::arm_after_ns(TICK_NS.load(Ordering::Relaxed));
    exception::irq_enable();
}

/// Sets up interrupts on a secondary CPU, with IRQs masked and
/// [`cpu::set_id`] done: the exception vectors, its GIC interface, the
/// SGIs and PPIs registered so far (the timer, the IPI), and its tick.
/// IRQs stay masked; the scheduler unmasks them.
pub fn init_cpu() {
    // SAFETY: as in `init`; the handlers are already registered.
    unsafe { exception::install() };
    let gic = GIC.get().expect("irq::init ran on the boot CPU");
    gic.init_cpu();
    for (id, f) in &HANDLERS {
        let intid = id.load(Ordering::Relaxed);
        if f.load(Ordering::Acquire) > CLAIMED && intid < 32 {
            gic.enable(intid);
        }
    }
    timer::arm_after_ns(TICK_NS.load(Ordering::Relaxed));
}

/// Ticks of CPU 0's timer since [`init`].
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Timer interrupts and IPIs taken by CPU `cpu`.
pub fn cpu_counts(cpu: usize) -> (u64, u64) {
    (
        CPU_TICKS[cpu].load(Ordering::Relaxed),
        CPU_IPIS[cpu].load(Ordering::Relaxed),
    )
}

/// The tick period in nanoseconds.
pub fn tick_ns() -> u64 {
    TICK_NS.load(Ordering::Relaxed)
}
