//! Preemptive kernel threads on one CPU.
//!
//! Each thread has its own stack. A thread runs until it yields (a
//! [`Rendez`] sleep yields through the [`Sched`] hook) or until the timer
//! interrupt calls [`Scheduler::preempt`]. The probe's cooperative scheduler
//! is the same design without the interrupt.
//!
//! Interrupt state: the thread table is only touched with IRQs masked, so
//! the IRQ handler can never find it locked. A voluntary switch saves DAIF
//! before switching and restores it after, so each thread gets its own mask
//! back. A preempted thread resumes inside its IRQ handler, whose `eret`
//! restores the interrupted DAIF. A new thread starts masked (it is first
//! entered from a switch) and unmasks in its trampoline.
//!
//! Limitation: a thread can be preempted while it holds a spinlock. On one
//! CPU the next thread that wants that lock spins until the following tick,
//! when the holder runs again, so this is slow but not a deadlock. K2 should
//! mask preemption around the platform lock facade.
//!
//! [`Rendez`]: purg_platform::sync::Rendez

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;

use purg_platform::sync::{Note, Sched};
use spin::Mutex;

use crate::exception::{IrqGuard, irq_enable, irq_restore, irq_save};
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

struct Thread {
    ctx: Context,
    /// Owns the stack; `None` for the boot thread.
    _stack: Option<Vec<u8>>,
    entry: Option<Box<dyn FnOnce() + Send>>,
    pid: u32,
    note: Option<Arc<Note>>,
    done: bool,
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
}

impl Threads {
    fn current_mut(&mut self) -> &mut Thread {
        let cur = self.current;
        self.all[cur].as_mut().expect("current thread exists")
    }
}

pub struct Scheduler {
    threads: Mutex<Threads>,
    stack_size: usize,
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
            }),
            stack_size,
        }
    }

    /// Makes the caller, on the boot stack, thread 0, and registers this
    /// scheduler as the [`Rendez`](purg_platform::sync::Rendez) hook.
    pub fn init(&'static self) {
        {
            let _g = IrqGuard::new();
            let mut t = self.threads.lock();
            t.all.push(Some(Box::new(Thread {
                ctx: Context::default(),
                _stack: None,
                entry: None,
                pid: 0,
                note: None,
                done: false,
            })));
            t.current = 0;
        }
        purg_platform::sync::set_sched(self);
    }

    /// Starts a thread running `f`. It first runs at the next switch.
    pub fn spawn(&'static self, f: Box<dyn FnOnce() + Send>) {
        let mut stack = vec![0u8; self.stack_size];
        let top = (stack.as_mut_ptr() as usize + self.stack_size) & !15;
        let mut ctx = Context::default();
        ctx.x[0] = self as *const Scheduler as u64;
        ctx.x[11] = purg_aarch64_thread_start as *const () as u64;
        ctx.sp = top as u64;
        let th = Box::new(Thread {
            ctx,
            _stack: Some(stack),
            entry: Some(f),
            pid: 0,
            note: None,
            done: false,
        });
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
    }

    /// Switches to the next ready thread, if any. IRQs must be masked.
    fn switch(&self) {
        let (from, to) = {
            let mut t = self.threads.lock();
            let cur = t.current;
            let dead = core::mem::take(&mut t.dead);
            for id in dead {
                if id == cur {
                    t.dead.push(id);
                } else {
                    t.all[id] = None;
                }
            }
            let Some(next) = t.ready.pop_front() else {
                return;
            };
            if t.current_mut().done {
                t.dead.push(cur);
            } else {
                t.ready.push_back(cur);
            }
            t.current = next;
            t.switches += 1;
            let from: *mut Context = &mut t.all[cur].as_mut().expect("current thread").ctx;
            let to: *const Context = &t.all[next].as_ref().expect("ready thread").ctx;
            (from, to)
        };
        // SAFETY: both Contexts are in boxed Threads that stay in the table
        // until a different thread has run (`dead` is reaped only then), the
        // table lock is released, and IRQs are masked so nothing else can
        // touch the table before the switch completes.
        unsafe { purg_aarch64_switch(from, to) };
    }

    /// Called from the timer IRQ handler, after the timer is re-armed and
    /// the interrupt is EOId: gives the CPU to the next ready thread.
    pub fn preempt(&self) {
        self.switch();
    }

    /// Switches the scheduler has made since boot.
    pub fn switches(&self) -> u64 {
        let _g = IrqGuard::new();
        self.threads.lock().switches
    }

    /// Number of threads that have not finished.
    pub fn live(&self) -> usize {
        let _g = IrqGuard::new();
        let t = self.threads.lock();
        t.all.iter().flatten().filter(|th| !th.done).count()
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
        self.switch();
        irq_restore(daif);
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
