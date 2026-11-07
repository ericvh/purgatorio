//! Cooperative kernel threads on the boot CPU: each kproc gets its own
//! stack, and a thread runs until it yields. [`Rendez`] yields through the
//! [`Sched`] hook while its condition is false, so a thread blocked on a pipe
//! or a wait lets the others run. There are no interrupts, so a thread never
//! loses the CPU while it holds a spinlock.
//!
//! [`Rendez`]: purg_platform::sync::Rendez

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;

use purg_platform::sync::Sched;
use spin::Mutex;

use crate::hw;

/// Stack for each kproc. The wasm guest's own stack is in its linear
/// memory; this holds the interpreter and the host calls it makes.
const STACK_SIZE: usize = 2 << 20;

/// Callee-saved state, laid out for `purg_switch`.
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
    /// Owns the thread's stack; `None` for the boot thread, which runs on
    /// the boot stack.
    _stack: Option<Vec<u8>>,
    entry: Option<Box<dyn FnOnce() + Send>>,
    pid: u32,
    done: bool,
}

struct Threads {
    /// Boxed so a thread's `Context` stays put while the table grows.
    all: Vec<Option<Box<Thread>>>,
    current: usize,
    ready: VecDeque<usize>,
    /// Exited threads whose stacks can be freed once another thread runs.
    dead: Vec<usize>,
}

pub struct Scheduler {
    threads: Mutex<Threads>,
}

pub static SCHED: Scheduler = Scheduler {
    threads: Mutex::new(Threads {
        all: Vec::new(),
        current: 0,
        ready: VecDeque::new(),
        dead: Vec::new(),
    }),
};

unsafe extern "C" {
    fn purg_switch(from: *mut Context, to: *const Context);
    fn purg_thread_start();
}

global_asm!(
    r#"
.global purg_switch
purg_switch:
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

.global purg_thread_start
purg_thread_start:
    mov     x29, xzr
    bl      purg_thread_main
1:  b       1b
"#
);

/// First code a new thread runs, on its own stack.
#[unsafe(no_mangle)]
extern "C" fn purg_thread_main() -> ! {
    let entry = {
        let mut t = SCHED.threads.lock();
        let cur = t.current;
        t.all[cur].as_mut().and_then(|th| th.entry.take())
    };
    if let Some(f) = entry {
        f();
    }
    SCHED.threads.lock().current_mut().done = true;
    loop {
        SCHED.yield_now();
    }
}

impl Threads {
    fn current_mut(&mut self) -> &mut Thread {
        let cur = self.current;
        self.all[cur].as_mut().expect("current thread exists")
    }
}

impl Scheduler {
    /// Makes the caller, on the boot stack, thread 0.
    pub fn init(&'static self) {
        let mut t = self.threads.lock();
        t.all.push(Some(Box::new(Thread {
            ctx: Context::default(),
            _stack: None,
            entry: None,
            pid: 0,
            done: false,
        })));
        t.current = 0;
        drop(t);
        purg_platform::sync::set_sched(self);
    }

    /// Starts a thread running `f`; it first runs when the caller yields.
    pub fn spawn(&self, f: Box<dyn FnOnce() + Send>) {
        let mut stack = vec![0u8; STACK_SIZE];
        let top = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15;
        let mut ctx = Context::default();
        ctx.x[11] = purg_thread_start as *const () as u64;
        ctx.sp = top as u64;
        let th = Box::new(Thread {
            ctx,
            _stack: Some(stack),
            entry: Some(f),
            pid: 0,
            done: false,
        });
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

    pub fn current_pid(&self) -> u32 {
        self.threads.lock().current_mut().pid
    }

    pub fn set_current_pid(&self, pid: u32) {
        self.threads.lock().current_mut().pid = pid;
    }
}

impl Sched for Scheduler {
    fn yield_now(&self) {
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
            let from: *mut Context = &mut t.all[cur].as_mut().expect("current thread").ctx;
            let to: *const Context = &t.all[next].as_ref().expect("ready thread").ctx;
            (from, to)
        };
        // SAFETY: both Contexts are in boxed Threads that stay in the table
        // until a different thread has run (`dead` is reaped only then), and
        // the table lock has been released.
        unsafe { purg_switch(from, to) };
    }

    fn monotonic_ns(&self) -> u64 {
        let hz = hw::counter_hz().max(1);
        (u128::from(hw::counter()) * 1_000_000_000 / u128::from(hz)) as u64
    }
}
