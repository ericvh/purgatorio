//! Where the memory goes: the reports behind `docs/footprint.md`.
//!
//! With `memstat` in the bootargs (`KERNEL_MEMSTAT=1 cargo xtask kernel`)
//! the kernel prints `kernel: mem ...` lines:
//!
//! - once after the heap is up: the RAM, the image's sections and the heap;
//! - in test mode, after each command and script: the heap figures, the
//!   peak since the line before, and the live kernel threads by name
//!   (`System::memstat`);
//! - interactively, whenever the heap or the thread count changed since
//!   the last look (once a second): the bytes in use, the peak since the
//!   last line, and the threads;
//! - at the end: the peak thread count and the deepest stacks.
//!
//! The lines carry numbers that change with every build, so `xtask kernel`
//! leaves them out when it compares the output. The heap's counters are
//! also in `/dev/memory`, in Inferno's format, without `memstat`; with it
//! the high-water mark there restarts at each line.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use purg_platform::sync::Rendez;

use crate::irq::SCHED;
use crate::kernel::{HEAP, boot_stack_used, println};

static ON: AtomicBool = AtomicBool::new(false);

/// The heap's peak over the whole run: [`run_peak`] restarts the heap's
/// own.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Stack for the sampler thread: it only formats a line.
const SAMPLER_STACK: usize = 32 << 10;

const fn kib(n: usize) -> usize {
    n.div_ceil(1024)
}

/// Turns the reports on. The boot stack has been painted already.
pub fn enable() {
    ON.store(true, Ordering::Release);
    SCHED.measure_stacks();
}

pub fn on() -> bool {
    ON.load(Ordering::Acquire)
}

/// The heap's peak since the last call and over the whole run, in bytes;
/// restarts the first.
pub fn run_peak() -> (usize, usize) {
    let (_, peak) = HEAP.used_and_peak();
    HEAP.reset_peak();
    let all = PEAK.load(Ordering::Relaxed).max(peak);
    PEAK.store(all, Ordering::Relaxed);
    (peak, all)
}

/// Prints the thread counts and the deepest stacks so far.
pub fn report(what: &str) {
    if !on() {
        return;
    }
    let t = SCHED.stats();
    println!(
        "kernel: mem {what}: threads {} now, {} at most, {} started; \
         deepest stacks: prog thread {} KiB, export worker {} KiB, other kproc {} KiB (of {} KiB), boot {} KiB",
        t.threads,
        t.peak_threads,
        t.spawned,
        kib(t.prog_stack),
        kib(t.export_stack),
        kib(t.kproc_stack),
        kib(t.stack_size),
        kib(boot_stack_used())
    );
}

/// Starts a thread that prints a line whenever the heap or the thread
/// count has changed, looking once a second.
pub fn sampler() {
    if !on() {
        return;
    }
    SCHED.spawn_sized(
        SAMPLER_STACK,
        Box::new(|| {
            let tick = Rendez::new();
            let mut last = (0, 0);
            loop {
                tick.tsleep(|| false, 1_000_000_000);
                let (used, _) = HEAP.used_and_peak();
                let threads = SCHED.live();
                if (used, threads) != last {
                    last = (used, threads);
                    let (peak, _) = run_peak();
                    println!(
                        "kernel: mem heap {} KiB in use, peak {} KiB since the last line, {threads} threads",
                        kib(used),
                        kib(peak)
                    );
                }
            }
        }),
    );
}
