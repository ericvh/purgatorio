//! The stress mode (`stress=SECS` in the bootargs): the sh corpus and the
//! conformance guests run over and over, concurrently, in many progs on
//! every CPU, for `SECS` seconds.
//!
//! `stressprogs=N` workers (default [`WORKERS`]) each take the jobs in
//! turn, starting at a different one, and run each with
//! [`purg_system::System::run_isolated`]: its own copy of the namespace,
//! its own `/tmp`, and a prog domain, so a run's leftovers are killed
//! without touching another's. Each run's output must match: a corpus
//! script's its `.out`, a guest's an empty exit status.
//!
//! Three checks:
//! - wrong output: every failed run is counted, and the first few are
//!   printed as they happen;
//! - deadlock: a watchdog (the thread that started the workers) fails the
//!   run if no job finishes for `stresswatch=SECS` seconds (default
//!   [`WATCH_SECS`]), after printing every kernel thread (where it is, how
//!   it last left a CPU, the locks it holds) and every prog;
//! - leaks: once the workers are done, the heap and the kernel threads
//!   must be back where they were before, within [`LEAK_LIMIT`], as after
//!   the test mode.
//!
//! Left out, because they use names that are global to the kernel, so
//! two copies would collide: the network guest (fixed ports), and the
//! corpus scripts that change state every prog shares ([`SHARED`]).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use purg_abi::mode;
use purg_core::Prog;
use purg_platform::sync::{Rendez, Sched};
use purg_styx::DMDIR;

use crate::irq::SCHED;
use crate::kernel::HEAP;
use crate::system::{SH_CORPUS, System};

/// Workers when the bootargs do not say.
pub const WORKERS: usize = 8;
/// Seconds without a finished job before the watchdog fires, when the
/// bootargs do not say.
pub const WATCH_SECS: u64 = 120;
/// Heap the whole run may leave in use: more than the test mode's, as
/// tables (the pid table's map, the RAM tree's) grow with the number of
/// runs at once.
const LEAK_LIMIT: u64 = 4 << 20;
/// Failed runs printed in full, as they happen.
const SHOWN: usize = 5;

const MS: u64 = 1_000_000;
const SEC: u64 = 1000 * MS;

/// One thing to run, and what it must print.
struct Job {
    argv: Vec<String>,
    /// The corpus script's expected output; `None` for a guest, which
    /// must exit with an empty status.
    want: Option<Vec<u8>>,
}

/// Corpus scripts that change state every prog shares: `plumb`, `mash`
/// and `netcs` post fixed names in `#s` (the plumber's ports, `mash`'s
/// history, `ndb/cs`'s and `ndb/dns`'s files), `netcmds` serves TFTP on
/// a fixed UDP port, and `modsig` puts a key in the factotum `#¤`, which
/// every mount that authenticates then signs with. `ping` is left out
/// for a different reason: it counts a reply as lost after a second,
/// and under the stress load a reply can take longer than that.
const SHARED: &[&str] = &["plumb", "mash", "netcs", "netcmds", "modsig", "ping"];

/// The lines of a failed guest's output that are printed: its `FAIL`
/// lines, or else its last lines.
const TAIL: usize = 8;

/// The guests, which print `ok` lines and must exit cleanly.
const GUESTS: &[&[&str]] = &[
    &["/bin/test/abi.wasm"],
    &["/bin/test/procs.wasm", "/tmp"],
    &["/bin/test/m3.wasm"],
    &["/bin/test/wasi.wasm"],
];

/// What the workers share.
struct Shared {
    sys: purg_system::System,
    jobs: Vec<Job>,
    deadline: u64,
    /// The run's length in ns.
    length: u64,
    stop: AtomicBool,
    runs: AtomicU64,
    failed: AtomicU64,
    /// Failures printed so far.
    shown: AtomicUsize,
    done: AtomicUsize,
    wake: Rendez,
}

impl Shared {
    fn now(&self) -> u64 {
        SCHED.monotonic_ns()
    }

    /// Runs job after job, from job `first` on, until the deadline.
    fn work(&self, first: usize, tmp: &str) {
        let mut i = first;
        while self.now() < self.deadline && !self.stop.load(Ordering::Acquire) {
            let job = &self.jobs[i % self.jobs.len()];
            i += 1;
            let argv: Vec<&str> = job.argv.iter().map(String::as_str).collect();
            let problem = match (self.sys.run_isolated(&argv, tmp), &job.want) {
                (Err(e), _) => Some(format!("error: {e}")),
                (Ok((status, got)), None) if !status.is_empty() => {
                    let got = String::from_utf8_lossy(&got);
                    let fails: Vec<&str> = got.lines().filter(|l| l.contains("FAIL")).collect();
                    let lines: Vec<&str> = got.lines().collect();
                    let shown = if fails.is_empty() {
                        &lines[lines.len().saturating_sub(TAIL)..]
                    } else {
                        &fails[..fails.len().min(TAIL)]
                    };
                    Some(format!("status {status:?}:\n{}", shown.join("\n")))
                }
                (Ok((_, got)), Some(want)) if got != *want => Some(format!(
                    "output differs\n--- got\n{}--- want\n{}---",
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(want)
                )),
                (Ok((status, _)), Some(_)) if !status.is_empty() => {
                    Some(format!("status {status:?}"))
                }
                _ => None,
            };
            if let Some(p) = problem {
                self.failed.fetch_add(1, Ordering::AcqRel);
                if self.shown.fetch_add(1, Ordering::AcqRel) < SHOWN {
                    let at = (self.now() + self.length).saturating_sub(self.deadline) / SEC;
                    self.sys.say(&format!(
                        "kernel: FAIL stress: at {at} s: {}: {p}",
                        argv.join(" ")
                    ));
                }
            }
            self.runs.fetch_add(1, Ordering::AcqRel);
            self.wake.wakeup();
        }
        self.done.fetch_add(1, Ordering::AcqRel);
        self.wake.wakeup();
    }
}

impl System {
    /// The jobs: the sh corpus, then the guests.
    fn stress_jobs(&self) -> Result<Vec<Job>, String> {
        let names = self
            .list(SH_CORPUS)
            .map_err(|e| format!("{SH_CORPUS}: {e}"))?;
        let mut jobs = Vec::new();
        let scripts = names.iter().filter_map(|n| n.strip_suffix(".sh"));
        for base in scripts.filter(|b| !SHARED.contains(b)) {
            let want = self
                .read_file(&format!("{SH_CORPUS}/{base}.out"))
                .unwrap_or_default();
            jobs.push(Job {
                argv: alloc::vec![String::from("sh"), format!("{SH_CORPUS}/{base}.sh")],
                want: Some(want),
            });
        }
        for g in GUESTS {
            jobs.push(Job {
                argv: g.iter().map(|a| String::from(*a)).collect(),
                want: None,
            });
        }
        Ok(jobs)
    }

    /// Every kernel thread and every prog, for the watchdog.
    fn dump_state(&self) {
        for t in SCHED.dump() {
            let at = match t.cpu {
                Some(c) => format!("running on cpu{c}"),
                None => String::from("ready"),
            };
            self.say(&format!(
                "kernel: stress: thread {:?} pid {} {at}, last left by {:?}, {} locks held, {} runs",
                t.name, t.pid, t.left, t.preempt, t.runs
            ));
        }
        let procs = &self.kernel.procs;
        for pid in procs.pids() {
            if let Some(p) = procs.get(pid) {
                self.say(&format!(
                    "kernel: stress: prog {} {} {} group {}",
                    p.pid,
                    p.name(),
                    p.state(),
                    p.group()
                ));
            }
        }
    }

    /// Runs the stress mode for `secs` seconds with `workers` workers and
    /// a `watch`-second watchdog; returns the number of failures.
    pub fn stress(&self, secs: u64, workers: usize, watch: u64) -> usize {
        let jobs = match self.stress_jobs() {
            Ok(j) => j,
            Err(e) => {
                self.say(&format!("kernel: FAIL stress: {e}"));
                return 1;
            }
        };
        let (heap, threads) = self.in_use();
        let names_before = SCHED.threads();
        let mut boot = Prog::new(self.kernel.clone(), self.pgrp.clone());
        for w in 0..workers {
            let dir = format!("/tmp/stress{w}");
            match boot.create(&dir, mode::OREAD, DMDIR | 0o777) {
                Ok(fd) => {
                    let _ = boot.close(fd);
                }
                Err(e) => {
                    self.say(&format!("kernel: FAIL stress: {dir}: {e}"));
                    return 1;
                }
            }
        }
        drop(boot);
        self.say(&format!(
            "kernel: stress: {} jobs, {workers} workers, {} cpus, {secs} s",
            jobs.len(),
            SCHED.online()
        ));
        let shared = Arc::new(Shared {
            sys: (**self).clone(),
            jobs,
            deadline: SCHED.monotonic_ns() + secs * SEC,
            length: secs * SEC,
            stop: AtomicBool::new(false),
            runs: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            shown: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            wake: Rendez::new(),
        });
        for w in 0..workers {
            let s = shared.clone();
            let first = w * s.jobs.len() / workers;
            SCHED.spawn_named(
                "stress",
                Box::new(move || s.work(first, &format!("/tmp/stress{w}"))),
            );
        }

        // The watchdog, and a progress line a minute.
        let start = shared.now();
        let (mut seen, mut since) = (0, start);
        let mut report = start + 60 * SEC;
        while shared.done.load(Ordering::Acquire) < workers {
            shared
                .wake
                .tsleep(|| shared.done.load(Ordering::Acquire) == workers, SEC);
            let now = shared.now();
            let runs = shared.runs.load(Ordering::Acquire);
            if runs != seen {
                (seen, since) = (runs, now);
            } else if now - since > watch * SEC {
                self.say(&format!(
                    "kernel: FAIL stress: no job finished in {watch} s ({runs} runs so far); the threads and progs:"
                ));
                self.dump_state();
                shared.stop.store(true, Ordering::Release);
                return 1;
            }
            if now >= report {
                report += 60 * SEC;
                let (used, _) = HEAP.used_and_peak();
                self.say(&format!(
                    "kernel: stress: {} s, {runs} runs, {} failed, heap {} KiB, {} threads",
                    (now - start) / SEC,
                    shared.failed.load(Ordering::Acquire),
                    used >> 10,
                    SCHED.live()
                ));
            }
        }

        let mut failed = 0;
        let runs = shared.runs.load(Ordering::Acquire);
        let bad = shared.failed.load(Ordering::Acquire);
        if bad != 0 {
            self.say(&format!("kernel: FAIL stress: {bad} of {runs} runs failed"));
            failed += 1;
        }
        drop(shared);
        // A killed prog's mount readers and export workers end a little
        // after it does.
        let pause = Rendez::new();
        let mut settle = 0;
        let (heap_now, threads_now) = loop {
            let (h, t) = self.in_use();
            if t <= threads || settle == 100 {
                break (h, t);
            }
            settle += 1;
            pause.tsleep(|| false, 100 * MS);
        };
        if threads_now > threads {
            let mut left = SCHED.threads();
            for t in &names_before {
                if let Some(i) = left.iter().position(|x| x == t) {
                    left.remove(i);
                }
            }
            for (name, pid) in left {
                self.say(&format!("kernel: stress: left thread {name:?} pid {pid}"));
            }
        }
        let grew = heap_now.saturating_sub(heap);
        let runs_by_cpu: Vec<String> = SCHED.cpu_runs()[..SCHED.online()]
            .iter()
            .map(|n| format!("{n}"))
            .collect();
        let v = if threads_now > threads {
            Err(format!(
                "stress: the runs left {} kernel threads running",
                threads_now - threads
            ))
        } else if grew > LEAK_LIMIT {
            Err(format!("stress: the runs left {grew} bytes of heap in use"))
        } else {
            Ok(format!(
                "stress: {runs} runs in {secs} s, {bad} failed, nothing left behind; switches by cpu {}; {} sleeps ended by the safety timeout",
                runs_by_cpu.join(" "),
                purg_platform::sync::rescued()
            ))
        };
        self.verdict(&mut failed, v);
        failed
    }
}
