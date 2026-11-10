//! K2 self-tests for preemption-safe locks, idle, and the
//! interrupt-driven console. Each runs without interactive input:
//! `cargo xtask kernel` types the console test's input.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use purg_aarch64::console::Pl011Console;
use purg_platform::Console;
use purg_platform::sync::{Mutex, Rendez, Sched};

use crate::irq::{self, SCHED, TICK_HZ};
use crate::kernel::{fail, println};

const MS: u64 = 1_000_000;

/// How long the lock test's workers run, in ticks.
const LOCK_TICKS: u64 = 20;
/// Iterations of busy work inside each critical section, so that most
/// ticks land while a worker holds the lock.
const CS_SPIN: u32 = 2000;

/// Set while a worker is inside the critical section.
static HELD: AtomicBool = AtomicBool::new(false);
/// Times a running worker found the lock held, which on one CPU means its
/// holder was switched out holding it.
static SEEN_HELD: AtomicU32 = AtomicU32::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static LOCK_DONE: AtomicU32 = AtomicU32::new(0);
static MIN_SHARE: AtomicU64 = AtomicU64::new(u64::MAX);

fn lock_worker(m: &Mutex<u64>, done: &Rendez, end: u64) {
    let mut mine = 0;
    while irq::ticks() < end {
        if HELD.load(Ordering::Acquire) {
            SEEN_HELD.fetch_add(1, Ordering::Relaxed);
        }
        let mut g = m.lock();
        HELD.store(true, Ordering::Release);
        let v = *g;
        for _ in 0..CS_SPIN {
            core::hint::spin_loop();
        }
        *g = v + 1;
        HELD.store(false, Ordering::Release);
        drop(g);
        mine += 1;
    }
    TOTAL.fetch_add(mine, Ordering::AcqRel);
    MIN_SHARE.fetch_min(mine, Ordering::AcqRel);
    LOCK_DONE.fetch_add(1, Ordering::AcqRel);
    done.wakeup();
}

/// Two threads increment a counter under a `purg_platform` Mutex, with
/// long critical sections, for `LOCK_TICKS` ticks. Timer preemptions that
/// land in a critical section must be deferred to its end, so a running
/// thread never finds the lock held; both threads must still get turns,
/// and the counter must equal the increments made.
pub fn lock_test() {
    let m = Arc::new(Mutex::new(0u64));
    let done = Rendez::new();
    let deferred = SCHED.deferred();
    let end = irq::ticks() + LOCK_TICKS;
    for _ in 0..2 {
        let (m, done) = (m.clone(), done.clone());
        SCHED.spawn(Box::new(move || lock_worker(&m, &done, end)));
    }
    let finished = done.tsleep(
        || LOCK_DONE.load(Ordering::Acquire) == 2,
        (LOCK_TICKS + 5 * TICK_HZ) * 1000 / TICK_HZ * MS,
    );
    if !finished {
        fail(format_args!("lock test workers did not finish"));
    }
    let count = *m.lock();
    let total = TOTAL.load(Ordering::Acquire);
    let seen = SEEN_HELD.load(Ordering::Acquire);
    let deferred = SCHED.deferred() - deferred;
    let min = MIN_SHARE.load(Ordering::Acquire);
    if count != total {
        fail(format_args!("lock lost updates: {count} != {total}"));
    }
    if seen != 0 {
        fail(format_args!(
            "a preempted thread held the lock {seen} times ({deferred} deferred)"
        ));
    }
    if deferred == 0 {
        fail(format_args!(
            "no preemption was deferred in {LOCK_TICKS} ticks"
        ));
    }
    if min == 0 {
        fail(format_args!(
            "a worker never got the lock ({count} increments)"
        ));
    }
    println!("kernel: a lock holder is never preempted, and waiters still get turns");
}

/// A `tsleep` with nothing else to run idles the CPU and still wakes when
/// its timeout passes.
pub fn tsleep_test() {
    let idles = SCHED.idles();
    let r = Rendez::new();
    let start = SCHED.monotonic_ns();
    let woke = r.tsleep(|| false, 50 * MS);
    let took = SCHED.monotonic_ns() - start;
    if woke {
        fail(format_args!("tsleep(50ms) reported its condition true"));
    }
    if !(50 * MS..300 * MS).contains(&took) {
        fail(format_args!("tsleep(50ms) took {} ms", took / MS));
    }
    if SCHED.idles() == idles {
        fail(format_args!("the cpu never idled during tsleep(50ms)"));
    }
    println!("kernel: tsleep(50ms) woke on time with the cpu idle");
}

/// How long to wait for `cargo xtask kernel` to type the input.
const INPUT_TIMEOUT_MS: u64 = 20_000;

/// The console reader's result: the line, then what the next read returned.
type Reading = Mutex<Option<(Vec<u8>, usize)>>;

/// A thread reads a line from the console, blocking until the UART
/// interrupt brings it, then reads again and must see the ^D that follows
/// as end of input. `cargo xtask kernel` types `hello\n^D` once it sees the
/// prompt line, a little later, so the reader really blocks, and the CPU
/// must idle meanwhile.
pub fn console_test(cons: &'static Pl011Console) {
    let line: Arc<Reading> = Arc::new(Mutex::new(None));
    let done = Rendez::new();
    {
        let (line, done) = (line.clone(), done.clone());
        SCHED.spawn(Box::new(move || {
            let mut got = Vec::new();
            while !got.contains(&b'\n') {
                let mut b = [0u8; 64];
                match cons.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&b[..n]),
                }
            }
            let mut b = [0u8; 64];
            let after = cons.read(&mut b).unwrap_or(usize::MAX);
            *line.lock() = Some((got, after));
            done.wakeup();
        }));
    }
    let idles = SCHED.idles();
    println!("kernel: console: waiting for input");
    if !done.tsleep(|| line.lock().is_some(), INPUT_TIMEOUT_MS * MS) {
        fail(format_args!("no console input in {INPUT_TIMEOUT_MS} ms"));
    }
    let Some((got, after)) = line.lock().take() else {
        fail(format_args!("console reader vanished"));
    };
    let Some(text) = got
        .strip_suffix(b"\n")
        .and_then(|l| core::str::from_utf8(l).ok())
    else {
        fail(format_args!("console read {got:?}, not a line"));
    };
    if after != 0 {
        fail(format_args!(
            "console: ^D read as {after} bytes, not end of input"
        ));
    }
    if SCHED.idles() == idles {
        fail(format_args!(
            "the cpu never idled waiting for console input"
        ));
    }
    if cons.dropped() != 0 {
        fail(format_args!("console dropped {} bytes", cons.dropped()));
    }
    println!("kernel: console read {text:?} then end of input");
}
