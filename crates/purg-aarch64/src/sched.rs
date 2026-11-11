//! Preemptive kernel threads on one or more CPUs.
//!
//! Each thread has its own stack. A thread runs until it yields (a
//! [`Rendez`] sleep yields through the [`Sched`] hook) or until the timer
//! interrupt calls [`Scheduler::preempt`]. The kernel and the probe both
//! use it; the probe runs it on one CPU, the kernel on every CPU it brings
//! up (`crate::smp`).
//!
//! # Run queue
//!
//! One run queue, shared by every CPU, under a spinlock taken with IRQs
//! masked. Any CPU runs any thread, so a preempted thread may resume on
//! another CPU. A queue per CPU with work stealing would contend less,
//! but the lock is held for a few hundred instructions per switch, there
//! are at most [`MAX_CPUS`] CPUs, and one queue balances the load by
//! itself with no migration policy to get wrong. The lock is the first
//! thing to split if switches start to contend.
//!
//! Each CPU has an idle thread, pinned to it and never on the queue. A
//! CPU runs it when it has nothing else: when its thread exits with
//! nothing ready, and at boot on a secondary CPU. The idle thread takes
//! the next ready thread, or waits for an interrupt.
//!
//! # Switching
//!
//! A switch picks the next thread under the lock, but the outgoing thread
//! goes back on the queue (or is freed, if it has exited) only once its
//! registers are saved: the incoming thread does that
//! ([`Scheduler::finish`]) as soon as the switch returns into it.
//! Otherwise another CPU could take the outgoing thread off the queue and
//! run it on the stack it is still being switched off.
//!
//! Interrupt state: the thread table is only touched with IRQs masked, so
//! an IRQ handler never finds it locked by its own CPU. A voluntary
//! switch saves DAIF before switching and restores it after, so each
//! thread gets its own mask back. A preempted thread resumes inside its
//! IRQ handler, perhaps on another CPU, whose `eret` restores the
//! interrupted DAIF. A new thread starts masked (it is first entered from
//! a switch) and unmasks in its trampoline.
//!
//! # Preemption and locks
//!
//! The platform lock facade calls [`Sched::preempt_disable`] and
//! [`Sched::preempt_enable`] around every critical section. The count
//! belongs to the thread, not to the CPU: the facade's locks are Inferno
//! qlocks, and a thread may sleep, and so move to another CPU, holding
//! one. The thread finds its count through `TPIDR_EL1`
//! ([`crate::cpu::thread`]), which follows it from CPU to CPU, so no IRQ
//! masking is needed. A timer preemption that finds the count non-zero is
//! deferred and happens when the count drops to zero, so a thread is
//! never preempted holding a facade lock. IRQs stay unmasked meanwhile.
//! With several CPUs a holder keeps running while its waiters block on
//! other CPUs.
//!
//! # Idle
//!
//! There is no blocked state; a sleeping thread stays runnable and polls
//! its condition, calling [`Sched::block`] when it is false. When more
//! threads have blocked in a row, with no interrupt or [`Sched::wakeup`]
//! in between, than there are threads that could still look (one per CPU,
//! and those ready), nothing can change until an interrupt or another
//! CPU's wakeup, so the blocker waits for one with `wfi`. [`IdleGate`]
//! makes that decision race-free across CPUs: a wakeup sends an IPI to
//! every CPU that is waiting or about to. The 100 Hz tick on every CPU
//! bounds a wait, so `tsleep` deadlines still pass on time.
//!
//! # Stacks
//!
//! A new stack is filled with [`PAINT`], so the deepest a thread went is
//! where the paint ends. [`Scheduler::stats`] reports the deepest stack,
//! separately for threads that ran a prog (wasmi and the host calls under
//! it) and for those that did not (the mount readers, the boot-time
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

use purg_platform::idle::IdleGate;
use purg_platform::sync::{Note, Sched};
use spin::Mutex;

use crate::cpu::{self, MAX_CPUS};
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

/// The stack of each CPU's idle thread: its loop, and an interrupt
/// handler's frames on top.
pub const IDLE_STACK: usize = 32 << 10;

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
    /// Threads that exist now, including the boot thread but not the idle
    /// threads.
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
    /// The deepest stack of a mount reader: a kproc named `mnt`, which
    /// reads a mounted server's replies.
    pub mnt_stack: usize,
}

/// How a thread last left its CPU, for [`Scheduler::dump`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// It has not run yet.
    New,
    /// It yielded.
    Yield,
    /// It blocked: its condition was false.
    Block,
    /// The timer preempted it.
    Preempt,
}

/// One thread, as [`Scheduler::dump`] reports it.
#[derive(Clone, Debug)]
pub struct ThreadInfo {
    pub name: String,
    pub pid: u32,
    /// The CPU it is running on, or `None` if it is waiting to run.
    pub cpu: Option<usize>,
    pub left: Left,
    /// Its preemption-disable count: the facade locks it holds.
    pub preempt: u32,
    /// Times it has been switched to.
    pub runs: u64,
}

/// A thread. Its address is in `TPIDR_EL1` while it runs.
struct Thread {
    ctx: Context,
    /// Owns the stack; `None` for the boot thread.
    stack: Option<Vec<u8>>,
    entry: Option<Box<dyn FnOnce() + Send>>,
    /// What the thread is for, for reports.
    name: String,
    pid: u32,
    note: Option<Arc<Note>>,
    /// The preemption-disable count. Only the thread itself changes it;
    /// an interrupt on the CPU it runs on reads it.
    preempt: AtomicU32,
    /// A timer preemption arrived while `preempt` was non-zero.
    pending: AtomicBool,
    done: bool,
    /// The CPU whose idle thread this is. An idle thread is never queued.
    idle_of: Option<usize>,
    left: Left,
    runs: u64,
}

impl Thread {
    /// Which of [`SchedStats`]'s deepest stacks this thread counts in:
    /// an export worker, a mount reader, one that ran a prog, or another
    /// kproc.
    fn class(&self) -> usize {
        match self.name.as_str() {
            "export" => 2,
            "mnt" => 3,
            _ => usize::from(self.pid == 0),
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
            preempt: AtomicU32::new(0),
            pending: AtomicBool::new(false),
            done: false,
            idle_of: None,
            left: Left::New,
            runs: 0,
        }
    }

    /// A thread that first runs `purg_aarch64_thread_start` on `stack`.
    fn fresh(sched: &Scheduler, stack: Vec<u8>, entry: Option<Box<dyn FnOnce() + Send>>) -> Self {
        let top = (stack.as_ptr() as usize + stack.len()) & !15;
        let mut ctx = Context::default();
        ctx.x[0] = sched as *const Scheduler as u64;
        ctx.x[11] = purg_aarch64_thread_start as *const () as u64;
        ctx.sp = top as u64;
        Thread::new(ctx, Some(stack), entry)
    }
}

/// A thread a CPU switched away from, whose registers were still being
/// saved: the next thread on that CPU requeues or frees it.
#[derive(Clone, Copy)]
struct Outgoing {
    id: usize,
    done: bool,
    /// It can run now (it was preempted or yielded, rather than blocked),
    /// so a waiting CPU should be interrupted to take it.
    runnable: bool,
}

const NONE: usize = usize::MAX;

struct Threads {
    /// Boxed so a thread's address stays put while the table grows.
    all: Vec<Option<Box<Thread>>>,
    /// The thread each CPU runs.
    current: [usize; MAX_CPUS],
    /// Each CPU's idle thread, [`NONE`] before it has one.
    idle: [usize; MAX_CPUS],
    /// What each CPU last switched away from, for [`Scheduler::finish`].
    outgoing: [Option<Outgoing>; MAX_CPUS],
    ready: VecDeque<usize>,
    /// Number of switches, for tests and `/dev/sysstat`-like reporting.
    switches: u64,
    /// Switches to a thread other than the idle thread, by CPU.
    cpu_runs: [u64; MAX_CPUS],
    peak: usize,
    spawned: u64,
    /// The deepest stacks of exited threads, by [`Thread::class`].
    deepest: [usize; 4],
}

impl Threads {
    /// Records how deep `th`'s stack went.
    fn measure(&mut self, th: &Thread) {
        if let Some(stack) = &th.stack {
            let class = th.class();
            self.deepest[class] = self.deepest[class].max(stack_used(stack));
        }
    }

    fn current_mut(&mut self, cpu: usize) -> &mut Thread {
        let cur = self.current[cpu];
        self.all[cur].as_mut().expect("current thread exists")
    }

    /// Puts `th` in a free slot and returns its id.
    fn insert(&mut self, th: Box<Thread>) -> usize {
        match self.all.iter().position(Option::is_none) {
            Some(i) => {
                self.all[i] = Some(th);
                i
            }
            None => {
                self.all.push(Some(th));
                self.all.len() - 1
            }
        }
    }

    /// Every thread but the idle threads.
    fn counted(&self) -> impl Iterator<Item = &Thread> + '_ {
        self.all
            .iter()
            .flatten()
            .map(|th| &**th)
            .filter(|th| th.idle_of.is_none())
    }

    /// The CPU thread `id` is running on.
    fn cpu_of(&self, id: usize) -> Option<usize> {
        self.current.iter().position(|&c| c == id)
    }
}

pub struct Scheduler {
    threads: Mutex<Threads>,
    stack_size: usize,
    /// CPUs running threads.
    online: AtomicUsize,
    /// When CPUs may wait, and which a wakeup must interrupt.
    gate: IdleGate,
    /// `fn(u64)` sending the reschedule IPI to a set of CPUs, as `usize`;
    /// 0 until `crate::irq` sets it.
    ipi: AtomicUsize,
    /// Preemptions deferred because the thread held a lock.
    deferred: AtomicU64,
    /// Times a CPU waited for an interrupt.
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
    sched.finish();
    let (entry, idle) = {
        let _g = IrqGuard::new();
        let cpu = cpu::id();
        let mut t = sched.threads.lock();
        let th = t.current_mut(cpu);
        (th.entry.take(), th.idle_of)
    };
    if let Some(cpu) = idle {
        sched.idle_loop(cpu);
    }
    irq_enable();
    if let Some(f) = entry {
        f();
    }
    {
        let _g = IrqGuard::new();
        let cpu = cpu::id();
        sched.threads.lock().current_mut(cpu).done = true;
    }
    loop {
        sched.yield_now();
    }
}

/// The running thread, through `TPIDR_EL1`; `None` before
/// [`Scheduler::init`].
fn running() -> Option<&'static Thread> {
    let p = cpu::thread();
    // SAFETY: the scheduler stores in TPIDR_EL1 only the address of the
    // boxed Thread it switches to, and frees a Thread only once it has
    // exited and another thread runs on the CPU that ran it last. Its
    // `preempt` and `pending` fields are atomics.
    (p != 0).then(|| unsafe { &*(p as *const Thread) })
}

impl Scheduler {
    /// A scheduler whose new threads get `stack_size`-byte stacks.
    pub const fn new(stack_size: usize) -> Scheduler {
        Scheduler {
            threads: Mutex::new(Threads {
                all: Vec::new(),
                current: [NONE; MAX_CPUS],
                idle: [NONE; MAX_CPUS],
                outgoing: [None; MAX_CPUS],
                ready: VecDeque::new(),
                switches: 0,
                cpu_runs: [0; MAX_CPUS],
                peak: 0,
                spawned: 0,
                deepest: [0; 4],
            }),
            stack_size,
            online: AtomicUsize::new(0),
            gate: IdleGate::new(),
            ipi: AtomicUsize::new(0),
            deferred: AtomicU64::new(0),
            idles: AtomicU64::new(0),
            measure: AtomicBool::new(false),
        }
    }

    /// Makes the caller, on the boot stack of CPU 0, thread 0, gives CPU 0
    /// its idle thread, and registers this scheduler as the
    /// [`Rendez`](purg_platform::sync::Rendez) and lock hook.
    pub fn init(&'static self) {
        cpu::set_id(0);
        let mut idle = Box::new(Thread::fresh(self, vec![PAINT; IDLE_STACK], None));
        idle.idle_of = Some(0);
        {
            let _g = IrqGuard::new();
            let mut t = self.threads.lock();
            let boot = t.insert(Box::new(Thread::new(Context::default(), None, None)));
            t.current[0] = boot;
            t.idle[0] = t.insert(idle);
            let th: *const Thread = &**t.all[boot].as_ref().expect("boot thread");
            cpu::set_thread(th as usize);
        }
        self.online.store(1, Ordering::Release);
        purg_platform::sync::set_sched(self);
    }

    /// Sets how the scheduler sends its IPI: `f` interrupts every CPU in
    /// the bit set it is given.
    pub fn set_ipi(&self, f: fn(u64)) {
        self.ipi.store(f as usize, Ordering::Release);
    }

    fn send_ipi(&self, mask: u64) {
        let p = self.ipi.load(Ordering::Acquire);
        if mask != 0 && p != 0 {
            // SAFETY: only `set_ipi` stores a non-zero value, an `fn(u64)`.
            let f = unsafe { core::mem::transmute::<usize, fn(u64)>(p) };
            f(mask);
        }
    }

    /// Interrupts the CPUs other than this one that are waiting, so they
    /// look at the run queue or their conditions again. IRQs must be
    /// masked.
    fn kick(&self, mask: u64) {
        self.send_ipi(mask & !(1 << cpu::id()));
    }

    /// Makes the idle thread of secondary CPU `cpu`, which the CPU starts
    /// on. Returns the top of its stack.
    pub fn add_cpu(&'static self, cpu: usize) -> usize {
        assert!(cpu > 0 && cpu < MAX_CPUS, "sched: no room for cpu {cpu}");
        let stack = vec![PAINT; IDLE_STACK];
        let top = (stack.as_ptr() as usize + stack.len()) & !15;
        let mut idle = Box::new(Thread::new(Context::default(), Some(stack), None));
        idle.idle_of = Some(cpu);
        let _g = IrqGuard::new();
        let mut t = self.threads.lock();
        let id = t.insert(idle);
        t.idle[cpu] = id;
        t.current[cpu] = id;
        top
    }

    /// Runs on secondary CPU `cpu`, on the stack [`Scheduler::add_cpu`]
    /// made, with IRQs masked and its interrupts set up: becomes its idle
    /// thread, counts itself online, calls `up` and runs threads from the
    /// queue for good.
    pub fn start_cpu(&'static self, cpu: usize, up: impl FnOnce()) -> ! {
        {
            let mut t = self.threads.lock();
            let th: *const Thread = &*t.current_mut(cpu);
            cpu::set_thread(th as usize);
        }
        self.online.fetch_add(1, Ordering::AcqRel);
        up();
        self.idle_loop(cpu)
    }

    /// CPUs running threads.
    pub fn online(&self) -> usize {
        self.online.load(Ordering::Acquire)
    }

    /// An idle thread's loop: run the next ready thread, or wait for an
    /// interrupt when there is none. A CPU that waits is in the gate's
    /// waiting set from before it looks at the queue until after the
    /// `wfi`, so a thread queued meanwhile brings an IPI.
    fn idle_loop(&'static self, cpu: usize) -> ! {
        loop {
            irq_disable();
            self.switch(Left::Yield);
            self.gate.announce(cpu);
            if self.threads.lock().ready.is_empty() {
                self.wfi();
            }
            self.gate.leave(cpu);
            self.take_irqs();
        }
    }

    /// Waits for an interrupt. IRQs must be masked: `wfi` wakes on a
    /// pending interrupt even so, and [`Scheduler::take_irqs`] takes it,
    /// so one that arrives just before the `wfi` is not missed.
    fn wfi(&self) {
        self.idles.fetch_add(1, Ordering::Relaxed);
        // SAFETY: waits for an interrupt; no memory or register effects.
        unsafe { asm!("dsb sy", "wfi", options(nostack, preserves_flags)) };
    }

    /// Unmasks IRQs long enough to take those pending, then masks them.
    fn take_irqs(&self) {
        irq_enable();
        // SAFETY: a context synchronization event, so a pending IRQ is
        // taken here, before IRQs are masked again.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
        irq_disable();
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
        let mut th = Box::new(Thread::fresh(self, vec![PAINT; stack_size], Some(f)));
        th.name = String::from(name);
        let _g = IrqGuard::new();
        {
            let mut t = self.threads.lock();
            let id = t.insert(th);
            t.ready.push_back(id);
            t.spawned += 1;
            t.peak = t.peak.max(t.counted().count());
        }
        self.gate.local_event();
        self.kick(self.gate.waiting());
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
        for th in t.counted() {
            if let Some(stack) = &th.stack {
                let class = th.class();
                deepest[class] = deepest[class].max(stack_used(stack));
            }
        }
        SchedStats {
            threads: t.counted().count(),
            peak_threads: t.peak.max(1),
            spawned: t.spawned,
            stack_size: self.stack_size,
            prog_stack: deepest[0],
            kproc_stack: deepest[1],
            export_stack: deepest[2],
            mnt_stack: deepest[3],
        }
    }

    /// Switches this CPU to the next ready thread, if any (or to its idle
    /// thread, if the running thread has exited), and returns once the
    /// running thread runs again. IRQs must be masked.
    fn switch(&self, how: Left) {
        let cpu = cpu::id();
        let (from, to) = {
            let mut t = self.threads.lock();
            let cur = t.current[cpu];
            let done = t.current_mut(cpu).done;
            let next = match t.ready.pop_front() {
                Some(n) => n,
                None if done => t.idle[cpu],
                None => return,
            };
            let th = t.current_mut(cpu);
            if let Some(stack) = &th.stack
                && stack.first_chunk() != Some(&[PAINT; 8])
            {
                panic!("kernel thread stack overflow (pid {})", th.pid);
            }
            th.left = how;
            th.pending.store(false, Ordering::Relaxed);
            t.outgoing[cpu] = Some(Outgoing {
                id: cur,
                done,
                runnable: how != Left::Block,
            });
            t.current[cpu] = next;
            t.switches += 1;
            let to = t.all[next].as_mut().expect("ready thread");
            to.runs += 1;
            let counted = to.idle_of.is_none();
            let to_th: *const Thread = &**to;
            let to: *const Context = &to.ctx;
            if counted {
                t.cpu_runs[cpu] += 1;
            }
            // From here to the switch nothing reads TPIDR_EL1: IRQs are
            // masked and no facade lock is taken.
            cpu::set_thread(to_th as usize);
            let from: *mut Context = &mut t.all[cur].as_mut().expect("current thread").ctx;
            (from, to)
        };
        // SAFETY: both Contexts are in boxed Threads that stay in the table:
        // `from` is not requeued or freed until the incoming thread's
        // `finish`, after its registers are saved, and `to` is current on
        // this CPU only. The table lock is released and IRQs are masked.
        unsafe { purg_aarch64_switch(from, to) };
        self.finish();
    }

    /// The second half of a switch, on the incoming thread: requeues the
    /// thread this CPU switched away from, or frees it if it exited. IRQs
    /// must be masked.
    fn finish(&self) {
        let cpu = cpu::id();
        let (dead, queued) = {
            let mut t = self.threads.lock();
            match t.outgoing[cpu].take() {
                None => (None, false),
                Some(o) if o.done => {
                    let th = t.all[o.id].take();
                    if let Some(th) = &th
                        && self.measure.load(Ordering::Relaxed)
                    {
                        t.measure(th);
                    }
                    (th, false)
                }
                Some(o) => {
                    let idle = t.all[o.id].as_ref().is_some_and(|th| th.idle_of.is_some());
                    if !idle {
                        t.ready.push_back(o.id);
                    }
                    // A thread that blocked would only look at its
                    // condition again on the CPU it wakes, and block.
                    (None, !idle && o.runnable)
                }
            }
        };
        // Its stack, note and name go outside the table lock.
        drop(dead);
        if queued {
            self.kick(self.gate.waiting());
        }
    }

    /// Called from the timer IRQ handler, after the timer is re-armed and
    /// the interrupt is EOId: gives the CPU to the next ready thread, or,
    /// if the running thread holds a lock, makes the switch happen when it
    /// releases its last one.
    pub fn preempt(&self) {
        if let Some(th) = running()
            && th.preempt.load(Ordering::Relaxed) != 0
        {
            th.pending.store(true, Ordering::Relaxed);
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.switch(Left::Preempt);
    }

    /// An event only this CPU can have missed, such as its own timer
    /// interrupt: lets blocked threads look again without waking other
    /// CPUs.
    pub fn local_event(&self) {
        self.gate.local_event();
    }

    /// Switches the scheduler has made since boot.
    pub fn switches(&self) -> u64 {
        let _g = IrqGuard::new();
        self.threads.lock().switches
    }

    /// Switches to a thread other than the idle thread, by CPU.
    pub fn cpu_runs(&self) -> [u64; MAX_CPUS] {
        let _g = IrqGuard::new();
        self.threads.lock().cpu_runs
    }

    /// Timer preemptions deferred because the running thread held a lock.
    pub fn deferred(&self) -> u64 {
        self.deferred.load(Ordering::Relaxed)
    }

    /// Times a CPU has waited for an interrupt.
    pub fn idles(&self) -> u64 {
        self.idles.load(Ordering::Relaxed)
    }

    /// Number of threads that have not finished, idle threads aside.
    pub fn live(&self) -> usize {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        t.counted().filter(|th| !th.done).count()
    }

    /// The name and pid of each thread that has not finished.
    pub fn threads(&self) -> Vec<(String, u32)> {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        t.counted()
            .filter(|th| !th.done)
            .map(|th| (th.name.clone(), th.pid))
            .collect()
    }

    /// Every thread that has not finished, idle threads aside, with where
    /// it is and how it last left a CPU: for a watchdog's report.
    pub fn dump(&self) -> Vec<ThreadInfo> {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        t.all
            .iter()
            .enumerate()
            .filter_map(|(id, th)| th.as_ref().map(|th| (id, th)))
            .filter(|(_, th)| th.idle_of.is_none() && !th.done)
            .map(|(id, th)| ThreadInfo {
                name: th.name.clone(),
                pid: th.pid,
                cpu: t.cpu_of(id),
                left: th.left,
                preempt: th.preempt.load(Ordering::Relaxed),
                runs: th.runs,
            })
            .collect()
    }

    pub fn current_pid(&self) -> u32 {
        let _g = IrqGuard::new();
        let cpu = cpu::id();
        self.threads.lock().current_mut(cpu).pid
    }

    pub fn set_current_pid(&self, pid: u32) {
        let _g = IrqGuard::new();
        let cpu = cpu::id();
        self.threads.lock().current_mut(cpu).pid = pid;
    }
}

impl Sched for Scheduler {
    fn yield_now(&self) {
        let daif = irq_save();
        self.gate.local_event();
        self.switch(Left::Yield);
        irq_restore(daif);
    }

    /// Only the threads running when the last event happened (one per
    /// CPU) can have checked their conditions before the event and
    /// blocked after it. So once more blocks than there are threads ready
    /// and CPUs have happened in a row, every thread has seen the latest
    /// state, and nothing can change until an interrupt or a wakeup.
    fn block(&self) {
        let daif = irq_save();
        let cpu = cpu::id();
        let run = self.gate.block();
        let threshold = self.threads.lock().ready.len() + self.online();
        if run > threshold && self.gate.enter(cpu, threshold) {
            self.wfi();
            self.gate.leave(cpu);
            self.take_irqs();
        }
        self.switch(Left::Block);
        irq_restore(daif);
    }

    /// Restarts the block count and interrupts the CPUs that are waiting,
    /// so the threads on them look again.
    fn wakeup(&self) {
        let _g = IrqGuard::new();
        let waiting = self.gate.wake();
        self.kick(waiting);
    }

    /// The count changes only on its own thread, and an interrupt that
    /// switches away restores it before returning, so a plain load and
    /// store are enough. The fences keep the critical section between the
    /// two calls; the lock itself orders memory between CPUs.
    fn preempt_disable(&self) {
        if let Some(th) = running() {
            let c = th.preempt.load(Ordering::Relaxed);
            th.preempt.store(c + 1, Ordering::Relaxed);
        }
        compiler_fence(Ordering::SeqCst);
    }

    fn preempt_enable(&self) {
        compiler_fence(Ordering::SeqCst);
        let Some(th) = running() else {
            return;
        };
        let c = th.preempt.load(Ordering::Relaxed);
        if c == 0 {
            return;
        }
        th.preempt.store(c - 1, Ordering::Relaxed);
        if c == 1 && th.pending.load(Ordering::Relaxed) {
            self.yield_now();
        }
    }

    fn monotonic_ns(&self) -> u64 {
        timer::ticks_to_ns(timer::counter())
    }

    fn note(&self) -> Option<Arc<Note>> {
        let _g = IrqGuard::new();
        let cpu = cpu::id();
        self.threads.lock().current_mut(cpu).note.clone()
    }

    fn set_note(&self, note: Option<Arc<Note>>) {
        let _g = IrqGuard::new();
        let cpu = cpu::id();
        // The old note is dropped outside the table lock.
        let old = core::mem::replace(&mut self.threads.lock().current_mut(cpu).note, note);
        drop(old);
    }
}
