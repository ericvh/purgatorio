//! Test mode's watchdog. A step of the test mode (a line `kernel: ...`
//! that `System::say` printed) that runs for [`STEP_WATCH_SECS`] seconds
//! gets a report on the console: the step, every kernel thread and how it
//! last left its CPU, every prog, and each disk's queue indices. It does
//! not stop the run: `xtask kernel` kills a run that does not finish and
//! prints its console, which then says where it stood (#187).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use purg_platform::sync::{Rendez, Sched};
use purg_system::System;

use crate::irq::SCHED;

/// Seconds one step may run before the watchdog reports it. The slowest
/// step under TCG on CI takes about 10 s; `xtask kernel` gives a whole
/// smoke run 180 s.
pub const STEP_WATCH_SECS: u64 = 60;

/// Stack for the watchdog thread: it formats lines.
const STACK: usize = 32 << 10;

pub struct Watchdog {
    stop: Arc<AtomicBool>,
    gone: Arc<AtomicBool>,
    wake: Rendez,
}

/// Starts the watchdog over `sys`; `disks` gives a line per disk.
pub fn start(sys: System, disks: Box<dyn Fn() -> Vec<String> + Send>) -> Watchdog {
    let stop = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let wake = Rendez::new();
    let (s, g, w) = (stop.clone(), gone.clone(), wake.clone());
    SCHED.spawn_named_sized(
        "watchdog",
        STACK,
        Box::new(move || {
            let platform = sys.kernel.platform.clone();
            let (mut seen, mut since, mut told) = (u64::MAX, platform.monotonic_ns(), false);
            while !s.load(Ordering::Acquire) {
                w.tsleep(|| s.load(Ordering::Acquire), 1_000_000_000);
                let (n, step) = sys.step_now();
                let now = platform.monotonic_ns();
                if n != seen {
                    (seen, since, told) = (n, now, false);
                    continue;
                }
                let secs = (now - since) / 1_000_000_000;
                if told || secs < STEP_WATCH_SECS {
                    continue;
                }
                told = true;
                let c = platform.console();
                let line = |s: String| {
                    let _ = c.write(s.as_bytes());
                    let _ = c.write(b"\n");
                };
                line(format!("kernel: watchdog: {step:?} has run for {secs} s"));
                for t in SCHED.dump() {
                    let at = match t.cpu {
                        Some(c) => format!("on cpu{c}"),
                        None if t.parked => String::from("parked"),
                        None => String::from("ready"),
                    };
                    line(format!(
                        "kernel: watchdog: thread {:?} pid {} {at}, left by {:?}, {} locks, {} runs",
                        t.name, t.pid, t.left, t.preempt, t.runs
                    ));
                }
                let procs = &sys.kernel.procs;
                for pid in procs.pids() {
                    if let Some(p) = procs.get(pid) {
                        line(format!(
                            "kernel: watchdog: prog {} {} {}",
                            p.pid,
                            p.name(),
                            p.state()
                        ));
                    }
                }
                for d in disks() {
                    line(format!("kernel: watchdog: {d}"));
                }
            }
            g.store(true, Ordering::Release);
        }),
    );
    Watchdog { stop, gone, wake }
}

impl Watchdog {
    /// Stops the watchdog and waits for its thread to end.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.wake.wakeup();
        while !self.gone.load(Ordering::Acquire) {
            SCHED.yield_now();
        }
    }
}
