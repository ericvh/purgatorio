//! K2 self-tests for preemption-safe locks, idle, and the
//! interrupt-driven console. Each runs without interactive input:
//! `cargo xtask kernel` types the console test's input.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use purg_aarch64::console::Pl011Console;
use purg_core::linedisc::ConsReader;
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

/// What `cargo xtask kernel` types for the cooked check: a line killed
/// with ^U, then a line with a typo fixed with DEL and ^H and a two-byte
/// character erased with one DEL, ended with CR; then ^D. (`xtask` holds
/// the same bytes as `KERNEL_INPUT`.)
///
/// `junk^U` `hexl` DEL ^H `llo é` `ü` DEL CR ^D
const COOKED_LINE: &str = "hello é\n";

/// What the terminal must have been sent for it.
const COOKED_ECHO: &[u8] =
    b"junk\x08 \x08\x08 \x08\x08 \x08\x08 \x08hexl\x08 \x08\x08 \x08llo \xc3\xa9\xc3\xbc\x08 \x08\n";

/// What `xtask` types for the raw check: bytes the cooked console would
/// have edited with or turned into something else.
const RAW_INPUT: &[u8] = b"a\x7f\x15\r\x04";

/// The console, keeping a copy of what is written to it.
struct Tee {
    cons: &'static Pl011Console,
    written: Mutex<Vec<u8>>,
}

impl Console for Tee {
    fn read(&self, buf: &mut [u8]) -> purg_platform::Result<usize> {
        self.cons.read(buf)
    }

    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        self.written.lock().extend_from_slice(buf);
        self.cons.write(buf)
    }
}

/// Runs `read` on its own thread and waits for its result, which needs
/// `xtask` to type something after the `waiting for {what}` line. The
/// thread must block for the input, with the CPU idle meanwhile.
fn typed<T: Send + 'static>(what: &str, read: impl FnOnce() -> T + Send + 'static) -> T {
    let result: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let done = Rendez::new();
    {
        let (result, done) = (result.clone(), done.clone());
        SCHED.spawn(Box::new(move || {
            let v = read();
            *result.lock() = Some(v);
            done.wakeup();
        }));
    }
    let idles = SCHED.idles();
    println!("kernel: console: waiting for {what}");
    if !done.tsleep(|| result.lock().is_some(), INPUT_TIMEOUT_MS * MS) {
        fail(format_args!("no console {what} in {INPUT_TIMEOUT_MS} ms"));
    }
    let Some(v) = result.lock().take() else {
        fail(format_args!("console reader vanished"));
    };
    if SCHED.idles() == idles {
        fail(format_args!(
            "the cpu never idled waiting for console {what}"
        ));
    }
    v
}

/// The console through the line discipline `#c/cons` uses
/// (`purg_core::linedisc`), with `cargo xtask kernel` typing.
///
/// Cooked: a thread reads a line, blocking until the UART interrupt
/// brings it, then reads again and must see the ^D that follows as end of
/// input. The line arrives edited, and the terminal was sent the echo.
///
/// Raw: the same bytes that edit a cooked line arrive as they are, in as
/// many reads as it takes, and nothing is echoed.
pub fn console_test(cons: &'static Pl011Console) {
    let tee = Arc::new(Tee {
        cons,
        written: Mutex::new(Vec::new()),
    });
    let reader = Arc::new(ConsReader::new());

    let (got, after) = {
        let (tee, reader) = (tee.clone(), reader.clone());
        typed("input", move || {
            let mut b = [0u8; 64];
            let n = reader.read(&*tee, &mut b).unwrap_or(0);
            let got = b[..n].to_vec();
            let after = reader.read(&*tee, &mut b).unwrap_or(usize::MAX);
            (got, after)
        })
    };
    if got != COOKED_LINE.as_bytes() {
        fail(format_args!(
            "console read {:?}, not {COOKED_LINE:?}",
            got.escape_ascii()
        ));
    }
    if after != 0 {
        fail(format_args!(
            "console: ^D read as {after} bytes, not end of input"
        ));
    }
    let echo = core::mem::take(&mut *tee.written.lock());
    if echo != COOKED_ECHO {
        fail(format_args!(
            "console echoed {:?}, not {:?}",
            echo.escape_ascii(),
            COOKED_ECHO.escape_ascii()
        ));
    }
    println!(
        "kernel: console read {:?} then end of input, and echoed the typing",
        COOKED_LINE.trim_end()
    );

    reader.set_raw(true);
    let got = {
        let (tee, reader) = (tee.clone(), reader.clone());
        typed("raw input", move || {
            let mut got = Vec::new();
            while got.len() < RAW_INPUT.len() {
                let mut b = [0u8; 64];
                match reader.read(&*tee, &mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&b[..n]),
                }
            }
            got
        })
    };
    if got != RAW_INPUT {
        fail(format_args!(
            "raw console read {:?}, not {:?}",
            got.escape_ascii(),
            RAW_INPUT.escape_ascii()
        ));
    }
    let echo = tee.written.lock();
    if !echo.is_empty() {
        fail(format_args!("raw console echoed {:?}", echo.escape_ascii()));
    }
    if cons.dropped() != 0 {
        fail(format_args!("console dropped {} bytes", cons.dropped()));
    }
    println!(
        "kernel: raw console read {} bytes unedited and echoed nothing",
        got.len()
    );
}

/// Threads in the SMP test: enough that every CPU gets some.
const SMP_THREADS: usize = 16;
/// Increments each makes.
const SMP_ROUNDS: u64 = 2000;

/// Many threads on every CPU increment a counter under a `purg_platform`
/// Mutex: none may be lost, and every online CPU must have run some of
/// them. Then a thread blocked on a `Rendez` with the CPUs idle must be
/// woken by another CPU's wakeup, which with more than one CPU takes an
/// IPI, long before its timeout.
pub fn smp_test() {
    use purg_aarch64::cpu;
    use purg_aarch64::exception::IrqGuard;
    static RAN_ON: AtomicU64 = AtomicU64::new(0);
    static FINISHED: AtomicU32 = AtomicU32::new(0);
    static TOTAL: AtomicU64 = AtomicU64::new(0);
    let m = Arc::new(Mutex::new(0u64));
    let done = Rendez::new();
    let want = (1u64 << SCHED.online()) - 1;
    // Under KVM the work is over in microseconds, before a CPU woken
    // from `wfi` has taken a thread, so the threads go on until every CPU
    // has run one (or a few seconds pass).
    let until = SCHED.monotonic_ns() + 5_000 * MS;
    for _ in 0..SMP_THREADS {
        let (m, done) = (m.clone(), done.clone());
        SCHED.spawn(Box::new(move || {
            let mut i = 0;
            while i < SMP_ROUNDS
                || (RAN_ON.load(Ordering::Acquire) != want && SCHED.monotonic_ns() < until)
            {
                *m.lock() += 1;
                if i % 64 == 0 {
                    let g = IrqGuard::new();
                    RAN_ON.fetch_or(1 << cpu::id(), Ordering::AcqRel);
                    drop(g);
                    SCHED.yield_now();
                }
                i += 1;
            }
            TOTAL.fetch_add(i, Ordering::AcqRel);
            FINISHED.fetch_add(1, Ordering::AcqRel);
            done.wakeup();
        }));
    }
    let all = SMP_THREADS as u32;
    if !done.tsleep(|| FINISHED.load(Ordering::Acquire) == all, 60_000 * MS) {
        fail(format_args!(
            "smp: {} of {SMP_THREADS} threads finished",
            FINISHED.load(Ordering::Acquire)
        ));
    }
    let count = *m.lock();
    let total = TOTAL.load(Ordering::Acquire);
    if count != total {
        fail(format_args!(
            "smp: a shared lock lost updates: {count} != {total}"
        ));
    }
    let online = SCHED.online();
    let ran = RAN_ON.load(Ordering::Acquire);
    if ran != want {
        fail(format_args!(
            "smp: threads ran on cpus {ran:#x}, not all of {want:#x}"
        ));
    }

    // A sleeper on its own thread, woken by this one.
    static FLAG: AtomicBool = AtomicBool::new(false);
    static WOKE: AtomicU64 = AtomicU64::new(0);
    let r = Rendez::new();
    {
        let r = r.clone();
        let done = done.clone();
        SCHED.spawn(Box::new(move || {
            let ok = r.tsleep(|| FLAG.load(Ordering::Acquire), 10_000 * MS);
            WOKE.store(if ok { SCHED.monotonic_ns() } else { 1 }, Ordering::Release);
            done.wakeup();
        }));
    }
    // Let the sleeper block and its CPU go idle.
    let pause = Rendez::new();
    pause.tsleep(|| false, 100 * MS);
    let t0 = SCHED.monotonic_ns();
    FLAG.store(true, Ordering::Release);
    r.wakeup();
    if !done.tsleep(|| WOKE.load(Ordering::Acquire) != 0, 20_000 * MS) {
        fail(format_args!("smp: the sleeper never ran"));
    }
    let woke = WOKE.load(Ordering::Acquire);
    if woke == 1 {
        fail(format_args!("smp: a wakeup from another cpu was missed"));
    }
    let took = woke.saturating_sub(t0);
    if took > 5_000 * MS {
        fail(format_args!("smp: a wakeup took {} ms", took / MS));
    }
    println!("kernel: smp: {online} cpus online");
    println!("kernel: threads on every cpu shared a lock without losing an update");
}
