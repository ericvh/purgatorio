//! Preemptive kernel threads on one CPU.
//!
//! Each thread has its own stack. A thread runs until it yields (a
//! [`Rendez`] sleep yields through the [`Sched`] hook) or until the timer
//! interrupt calls [`Scheduler::preempt`]. The kernel and the probe both
//! use it.
//!
//! Interrupt state: the thread table is only touched with IRQs masked, so
//! the IRQ handler can never find it locked. A voluntary switch saves DAIF
//! before switching and restores it after, so each thread gets its own mask
//! back. A preempted thread resumes inside its IRQ handler, whose `eret`
//! restores the interrupted DAIF. A new thread starts masked (it is first
//! entered from a switch) and unmasks in its trampoline.
//!
//! Preemption and locks: the platform lock facade calls
//! [`Sched::preempt_disable`] and [`Sched::preempt_enable`] around every
//! critical section. The count is per thread: [`Scheduler::switch`] saves
//! it with the thread, so a thread may sleep holding a lock. A timer
//! preemption that finds the count non-zero is deferred and happens when
//! the count drops to zero, so a thread is never preempted holding a
//! facade lock. IRQs stay unmasked meanwhile.
//!
//! Idle: there is no blocked state; a sleeping thread stays on the run
//! queue and polls its condition, calling [`Sched::block`] when it is
//! false. When every thread has blocked in a row with no interrupt,
//! [`Sched::wakeup`] or other switch in between, nothing can change until
//! an interrupt arrives, so the next blocker waits for one with `wfi`. The
//! 100 Hz tick bounds that wait, so `tsleep` deadlines still pass on time.
//!
//! Stacks: a new stack is filled with [`PAINT`], so the deepest a thread
//! went is where the paint ends. [`Scheduler::stats`] reports the deepest
//! stack, separately for threads that ran a prog (wasmi and the host calls
//! under it) and for those that did not (the mount readers, the boot-time
//! workers). Exited threads are only scanned once
//! [`Scheduler::measure_stacks`] has been called. The stacks are heap
//! blocks with no guard page, so every switch checks that the lowest word
//! of the outgoing thread's stack is still paint and panics if it is not:
//! a tripwire for an overflow, not a guarantee.
//!
//! [`Rendez`]: purg_platform::sync::Rendez

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, compiler_fence};

use purg_platform::sync::{Note, Sched};
use spin::Mutex;

use crate::exception::{IrqGuard, irq_disable, irq_enable, irq_restore, irq_save};
use crate::timer;

/// Callee-saved state, laid out for `purg_aarch64_switch`. A voluntary
/// switch is a function call, so the AAPCS64 callee-saved registers are all
/// that must survive; a preemption has already saved everything else in
/// its trap frame on the thread's stack.
#[repr(C)]
#[derive(Default)]
struct Context {
    /// x19..x28, x29 (frame pointer), x30 (link register).
    x: [u64; 12],
    sp: u64,
    /// d8..d15.
    d: [u64; 8],
}

/// The byte a new stack is filled with.
const PAINT: u8 = 0xa5;

/// Bytes of `stack` used at its deepest: everything above the lowest word
/// that is no longer paint.
fn stack_used(stack: &[u8]) -> usize {
    const WORD: usize = core::mem::size_of::<u64>();
    let paint = [PAINT; WORD];
    let (words, _) = stack.as_chunks::<WORD>();
    let clean = words.iter().take_while(|w| **w == paint).count();
    stack.len() - clean * WORD
}

/// What the scheduler has seen, for the footprint reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct SchedStats {
    /// Threads that exist now, including the boot thread.
    pub threads: usize,
    /// The most threads that existed at once.
    pub peak_threads: usize,
    /// Threads ever started.
    pub spawned: u64,
    /// The default stack size.
    pub stack_size: usize,
    /// The deepest stack, in bytes, of a thread that ran a prog.
    pub prog_stack: usize,
    /// The deepest stack of a thread that ran no prog.
    pub kproc_stack: usize,
    /// The deepest stack of an export worker: a kproc named `export`,
    /// which serves Styx requests through the exporting prog's namespace.
    pub export_stack: usize,
}

struct Thread {
    ctx: Context,
    /// Owns the stack; `None` for the boot thread.
    stack: Option<Vec<u8>>,
    entry: Option<Box<dyn FnOnce() + Send>>,
    /// What the thread is for, for reports.
    name: String,
    pid: u32,
    note: Option<Arc<Note>>,
    /// The thread's preemption-disable count while it is switched out.
    preempt: u32,
    done: bool,
}

impl Thread {
    /// Which of [`SchedStats`]'s deepest stacks this thread counts in:
    /// an export worker, one that ran a prog, or another kproc.
    fn class(&self) -> usize {
        if self.name == "export" {
            2
        } else {
            usize::from(self.pid == 0)
        }
    }

    fn new(ctx: Context, stack: Option<Vec<u8>>, entry: Option<Box<dyn FnOnce() + Send>>) -> Self {
        Thread {
            ctx,
            stack,
            entry,
            name: String::new(),
            pid: 0,
            note: None,
            preempt: 0,
            done: false,
        }
    }
}

struct Threads {
    /// Boxed so a thread's `Context` stays put while the table grows.
    all: Vec<Option<Box<Thread>>>,
    current: usize,
    ready: VecDeque<usize>,
    /// Exited threads, freed once another thread is running.
    dead: Vec<usize>,
    /// Number of switches, for tests and `/dev/sysstat`-like reporting.
    switches: u64,
    peak: usize,
    spawned: u64,
    /// The deepest stacks of exited threads: those that ran a prog, and
    /// the rest.
    deepest: [usize; 3],
}

impl Threads {
    /// Records how deep `th`'s stack went.
    fn measure(&mut self, th: &Thread) {
        if let Some(stack) = &th.stack {
            let class = th.class();
            self.deepest[class] = self.deepest[class].max(stack_used(stack));
        }
    }

    fn current_mut(&mut self) -> &mut Thread {
        let cur = self.current;
        self.all[cur].as_mut().expect("current thread exists")
    }
}

pub struct Scheduler {
    threads: Mutex<Threads>,
    stack_size: usize,
    /// The running thread's preemption-disable count.
    preempt_count: AtomicU32,
    /// A timer preemption arrived while `preempt_count` was non-zero.
    preempt_pending: AtomicBool,
    /// [`Sched::block`] calls in a row with no interrupt, wakeup or other
    /// switch in between.
    blocked_run: AtomicUsize,
    /// Preemptions deferred because the thread held a lock.
    deferred: AtomicU64,
    /// Times the CPU waited for an interrupt with nothing to run.
    idles: AtomicU64,
    /// Whether exited threads' stacks are scanned (see the module docs).
    measure: AtomicBool,
}

unsafe extern "C" {
    fn purg_aarch64_switch(from: *mut Context, to: *const Context);
    fn purg_aarch64_thread_start();
}

global_asm!(
    r#"
.global purg_aarch64_switch
purg_aarch64_switch:
    stp     x19, x20, [x0, #0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x9, sp
    str     x9, [x0, #96]
    stp     d8, d9, [x0, #104]
    stp     d10, d11, [x0, #120]
    stp     d12, d13, [x0, #136]
    stp     d14, d15, [x0, #152]

    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x9, [x1, #96]
    mov     sp, x9
    ldp     d8, d9, [x1, #104]
    ldp     d10, d11, [x1, #120]
    ldp     d12, d13, [x1, #136]
    ldp     d14, d15, [x1, #152]
    ret

.global purg_aarch64_thread_start
purg_aarch64_thread_start:
    mov     x29, xzr
    mov     x0, x19
    bl      purg_aarch64_thread_main
1:  b       1b
"#
);

/// First code a new thread runs, on its own stack, with IRQs masked. `x19`
/// of its initial context carries the scheduler.
#[unsafe(no_mangle)]
extern "C" fn purg_aarch64_thread_main(sched: &'static Scheduler) -> ! {
    let entry = {
        let _g = IrqGuard::new();
        let mut t = sched.threads.lock();
        let cur = t.current;
        t.all[cur].as_mut().and_then(|th| th.entry.take())
    };
    irq_enable();
    if let Some(f) = entry {
        f();
    }
    {
        let _g = IrqGuard::new();
        sched.threads.lock().current_mut().done = true;
    }
    loop {
        sched.yield_now();
    }
}

impl Scheduler {
    /// A scheduler whose new threads get `stack_size`-byte stacks.
    pub const fn new(stack_size: usize) -> Scheduler {
        Scheduler {
            threads: Mutex::new(Threads {
                all: Vec::new(),
                current: 0,
                ready: VecDeque::new(),
                dead: Vec::new(),
                switches: 0,
                peak: 0,
                spawned: 0,
                deepest: [0; 3],
            }),
            stack_size,
            preempt_count: AtomicU32::new(0),
            preempt_pending: AtomicBool::new(false),
            blocked_run: AtomicUsize::new(0),
            deferred: AtomicU64::new(0),
            idles: AtomicU64::new(0),
            measure: AtomicBool::new(false),
        }
    }

    /// Makes the caller, on the boot stack, thread 0, and registers this
    /// scheduler as the [`Rendez`](purg_platform::sync::Rendez) and lock
    /// hook.
    pub fn init(&'static self) {
        {
            let _g = IrqGuard::new();
            let mut t = self.threads.lock();
            t.all
                .push(Some(Box::new(Thread::new(Context::default(), None, None))));
            t.current = 0;
        }
        purg_platform::sync::set_sched(self);
    }

    /// Starts a thread running `f`. It first runs at the next switch.
    pub fn spawn(&'static self, f: Box<dyn FnOnce() + Send>) {
        self.spawn_named("", f);
    }

    /// [`Scheduler::spawn`], with a name for [`Scheduler::threads`].
    pub fn spawn_named(&'static self, name: &str, f: Box<dyn FnOnce() + Send>) {
        self.spawn_named_sized(name, self.stack_size, f);
    }

    /// [`Scheduler::spawn`] with a stack of `stack_size` bytes.
    pub fn spawn_sized(&'static self, stack_size: usize, f: Box<dyn FnOnce() + Send>) {
        self.spawn_named_sized("", stack_size, f);
    }

    /// [`Scheduler::spawn_named`] with a stack of `stack_size` bytes.
    pub fn spawn_named_sized(
        &'static self,
        name: &str,
        stack_size: usize,
        f: Box<dyn FnOnce() + Send>,
    ) {
        let mut stack = vec![PAINT; stack_size];
        let top = (stack.as_mut_ptr() as usize + stack_size) & !15;
        let mut ctx = Context::default();
        ctx.x[0] = self as *const Scheduler as u64;
        ctx.x[11] = purg_aarch64_thread_start as *const () as u64;
        ctx.sp = top as u64;
        let mut th = Box::new(Thread::new(ctx, Some(stack), Some(f)));
        th.name = String::from(name);
        let _g = IrqGuard::new();
        let mut t = self.threads.lock();
        let id = match t.all.iter().position(Option::is_none) {
            Some(i) => {
                t.all[i] = Some(th);
                i
            }
            None => {
                t.all.push(Some(th));
                t.all.len() - 1
            }
        };
        t.ready.push_back(id);
        t.spawned += 1;
        t.peak = t.peak.max(t.all.iter().flatten().count());
        self.blocked_run.store(0, Ordering::Relaxed);
    }

    /// Scans the stack of every thread that exits from now on, for
    /// [`Scheduler::stats`]. The scan runs with IRQs masked and reads the
    /// unused part of the stack, so it is off unless asked for.
    pub fn measure_stacks(&self) {
        self.measure.store(true, Ordering::Relaxed);
    }

    /// Thread counts and the deepest stacks: of the live threads, and of
    /// those that exited since [`Scheduler::measure_stacks`].
    pub fn stats(&self) -> SchedStats {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        let mut deepest = t.deepest;
        for th in t.all.iter().flatten() {
            if let Some(stack) = &th.stack {
                let class = th.class();
                deepest[class] = deepest[class].max(stack_used(stack));
            }
        }
        SchedStats {
            threads: t.all.iter().flatten().count(),
            peak_threads: t.peak.max(1),
            spawned: t.spawned,
            stack_size: self.stack_size,
            prog_stack: deepest[0],
            kproc_stack: deepest[1],
            export_stack: deepest[2],
        }
    }

    /// Switches to the next ready thread, if any, and returns once this
    /// thread runs again. IRQs must be masked.
    fn switch(&self) {
        let (from, to) = {
            let mut t = self.threads.lock();
            let cur = t.current;
            let dead = core::mem::take(&mut t.dead);
            for id in dead {
                if id == cur {
                    t.dead.push(id);
                } else if let Some(th) = t.all[id].take()
                    && self.measure.load(Ordering::Relaxed)
                {
                    t.measure(&th);
                }
            }
            let Some(next) = t.ready.pop_front() else {
                return;
            };
            let count = self.preempt_count.load(Ordering::Relaxed);
            let th = t.current_mut();
            if let Some(stack) = &th.stack
                && stack.first_chunk() != Some(&[PAINT; 8])
            {
                panic!("kernel thread stack overflow (pid {})", th.pid);
            }
            th.preempt = count;
            if th.done {
                t.dead.push(cur);
            } else {
                t.ready.push_back(cur);
            }
            t.current = next;
            t.switches += 1;
            let to = t.all[next].as_ref().expect("ready thread");
            self.preempt_count.store(to.preempt, Ordering::Relaxed);
            self.preempt_pending.store(false, Ordering::Relaxed);
            let to: *const Context = &to.ctx;
            let from: *mut Context = &mut t.all[cur].as_mut().expect("current thread").ctx;
            (from, to)
        };
        // SAFETY: both Contexts are in boxed Threads that stay in the table
        // until a different thread has run (`dead` is reaped only then), the
        // table lock is released, and IRQs are masked so nothing else can
        // touch the table before the switch completes.
        unsafe { purg_aarch64_switch(from, to) };
    }

    /// Waits for an interrupt and takes it. IRQs must be masked: `wfi`
    /// wakes on a pending interrupt even so, and unmasking afterwards takes
    /// it, so one that arrives just before the `wfi` is not missed.
    fn idle(&self) {
        self.idles.fetch_add(1, Ordering::Relaxed);
        // SAFETY: waits for an interrupt; no memory or register effects.
        unsafe { asm!("dsb sy", "wfi", options(nostack, preserves_flags)) };
        irq_enable();
        // SAFETY: a context synchronization event, so the pending IRQ is
        // taken here, before IRQs are masked again.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
        irq_disable();
        self.blocked_run.store(0, Ordering::Relaxed);
    }

    /// Called from the timer IRQ handler, after the timer is re-armed and
    /// the interrupt is EOId: gives the CPU to the next ready thread, or,
    /// if the running thread holds a lock, makes the switch happen when it
    /// releases its last one.
    pub fn preempt(&self) {
        if self.preempt_count.load(Ordering::Relaxed) != 0 {
            self.preempt_pending.store(true, Ordering::Relaxed);
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.blocked_run.store(0, Ordering::Relaxed);
        self.switch();
    }

    /// Switches the scheduler has made since boot.
    pub fn switches(&self) -> u64 {
        let _g = IrqGuard::new();
        self.threads.lock().switches
    }

    /// Timer preemptions deferred because the running thread held a lock.
    pub fn deferred(&self) -> u64 {
        self.deferred.load(Ordering::Relaxed)
    }

    /// Times the CPU has waited for an interrupt with every thread blocked.
    pub fn idles(&self) -> u64 {
        self.idles.load(Ordering::Relaxed)
    }

    /// Number of threads that have not finished.
    pub fn live(&self) -> usize {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        t.all.iter().flatten().filter(|th| !th.done).count()
    }

    /// The name and pid of each thread that has not finished.
    pub fn threads(&self) -> Vec<(String, u32)> {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        t.all
            .iter()
            .flatten()
            .filter(|th| !th.done)
            .map(|th| (th.name.clone(), th.pid))
            .collect()
    }

    pub fn current_pid(&self) -> u32 {
        let _g = IrqGuard::new();
        self.threads.lock().current_mut().pid
    }

    pub fn set_current_pid(&self, pid: u32) {
        let _g = IrqGuard::new();
        self.threads.lock().current_mut().pid = pid;
    }
}

impl Sched for Scheduler {
    fn yield_now(&self) {
        let daif = irq_save();
        self.blocked_run.store(0, Ordering::Relaxed);
        self.switch();
        irq_restore(daif);
    }

    /// Only the thread that was running when the last event happened can
    /// have checked its condition before the event and blocked after it.
    /// So once one more block than there are runnable threads has happened
    /// in a row, every thread has seen the latest state, and nothing can
    /// change until an interrupt.
    fn block(&self) {
        let daif = irq_save();
        let run = self.blocked_run.fetch_add(1, Ordering::Relaxed) + 1;
        let runnable = self.threads.lock().ready.len() + 1;
        if run > runnable {
            self.idle();
        }
        self.switch();
        irq_restore(daif);
    }

    fn wakeup(&self) {
        self.blocked_run.store(0, Ordering::Relaxed);
    }

    /// The count changes only on its own thread, and an interrupt that
    /// switches away restores it before returning, so a plain load and
    /// store are enough on one CPU. The fences keep the critical section
    /// between the two calls.
    fn preempt_disable(&self) {
        let c = self.preempt_count.load(Ordering::Relaxed);
        self.preempt_count.store(c + 1, Ordering::Relaxed);
        compiler_fence(Ordering::SeqCst);
    }

    fn preempt_enable(&self) {
        compiler_fence(Ordering::SeqCst);
        let c = self.preempt_count.load(Ordering::Relaxed);
        if c == 0 {
            return;
        }
        self.preempt_count.store(c - 1, Ordering::Relaxed);
        if c == 1 && self.preempt_pending.load(Ordering::Relaxed) {
            self.yield_now();
        }
    }

    fn monotonic_ns(&self) -> u64 {
        timer::ticks_to_ns(timer::counter())
    }

    fn note(&self) -> Option<Arc<Note>> {
        let _g = IrqGuard::new();
        self.threads.lock().current_mut().note.clone()
    }

    fn set_note(&self, note: Option<Arc<Note>>) {
        let _g = IrqGuard::new();
        self.threads.lock().current_mut().note = note;
    }
}
