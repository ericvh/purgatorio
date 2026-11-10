//! Where the memory goes: the reports behind `docs/footprint.md`.
//!
//! With `memstat` in the bootargs the kernel prints `kernel: mem: ...`
//! lines:
//!
//! - once after the heap is up: the RAM, the image's sections and the heap;
//! - in test mode, after each command and script: the heap's peak above
//!   where it stood before the run, which is what that run cost;
//! - interactively, whenever the heap or the thread count changed since
//!   the last look (once a second): the bytes in use, the peak since the
//!   last line, and the threads;
//! - at the end: the totals, the peak thread count and the deepest stacks.
//!
//! The lines carry numbers that change with every build, so `memstat` is
//! not part of `cargo xtask kernel`. The heap's counters are also in
//! `/dev/memory`, in Inferno's format, without `memstat`.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, Ordering};

use purg_platform::sync::Rendez;

use crate::irq::SCHED;
use crate::kernel::{HEAP, boot_stack_used, println};

static ON: AtomicBool = AtomicBool::new(false);

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

/// Measures one run: the heap's peak above its level at [`Meter::start`].
pub struct Meter {
    used: usize,
}

impl Meter {
    /// `None` unless `memstat` is on.
    pub fn start() -> Option<Meter> {
        if !on() {
            return None;
        }
        HEAP.reset_high_water();
        Some(Meter {
            used: HEAP.stats().used,
        })
    }

    pub fn report(self, what: &str) {
        let s = HEAP.stats();
        println!(
            "kernel: mem: {what}: peak +{} KiB (heap {} -> {} KiB), {} threads at most",
            kib(s.high_water.saturating_sub(self.used)),
            kib(self.used),
            kib(s.high_water),
            SCHED.stats().peak_threads
        );
    }
}

/// Prints the totals so far.
pub fn report(what: &str) {
    if !on() {
        return;
    }
    let h = HEAP.stats();
    let t = SCHED.stats();
    println!(
        "kernel: mem: {what}: heap {} KiB in use of {} KiB, {} allocs, {} frees; \
         threads {} now, {} at most, {} started",
        kib(h.used),
        kib(h.size),
        h.allocs,
        h.frees,
        t.threads,
        t.peak_threads,
        t.spawned
    );
    println!(
        "kernel: mem: {what}: deepest stacks: prog thread {} KiB, other kproc {} KiB (of {} KiB), boot {} KiB",
        kib(t.prog_stack),
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
                let h = HEAP.stats();
                let t = SCHED.stats();
                if (h.used, t.threads) != last {
                    last = (h.used, t.threads);
                    println!(
                        "kernel: mem: heap {} KiB in use, peak {} KiB since the last line, {} threads",
                        kib(h.used),
                        kib(h.high_water),
                        t.threads
                    );
                    HEAP.reset_high_water();
                }
            }
        }),
    );
}
