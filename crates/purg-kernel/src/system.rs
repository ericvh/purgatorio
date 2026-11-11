//! The shared core on metal (K2): the kernel's `Platform` (a
//! `purg_aarch64::platform::MetalPlatform`), the boot
//! namespace, and the programs the kernel runs: the conformance guests and
//! the `sh` corpus in test mode, or `sh -l` on the console.
//!
//! The namespace is the emu's: `#/` is the root, `#U` (here a RAM tree
//! seeded from the initrd) is unioned after it and takes creates, `#c` is
//! before `/dev`, and `#e`, `#p`, `#s`, `#d` are on `/env`, `/prog`,
//! `/chan` and `/fd`, `#I` (smoltcp over virtio-net, see [`net`]) is on
//! `/net`, and the kernel's devices (`#r`, `#t`; see [`crate::devices`])
//! are after `/dev`. A host directory over virtio-9p (`#9`, see
//! [`host9p`]) is mounted on `/n/host` when QEMU has one, and the virtio-blk
//! disks are `#S`, after `/dev`, with a purgfs on one of them bound on
//! `/n/disk` (`#K`, see [`disk`]). There are no host commands.

#[cfg(feature = "blk")]
pub mod disk;
#[cfg(not(feature = "blk"))]
#[path = "system/nodisk.rs"]
pub mod disk;
#[cfg(feature = "p9")]
pub mod host9p;
#[cfg(not(feature = "p9"))]
#[path = "system/nohost9p.rs"]
pub mod host9p;
#[cfg(feature = "net")]
pub mod net;
#[cfg(not(feature = "net"))]
#[path = "system/nonet.rs"]
pub mod net;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use purg_aarch64::platform::MetalPlatform;
use purg_core::devram::RamTree;
use purg_core::{Dev, Error, Platform};
use purg_platform::Console;
use purg_system::Boot;
use purg_vm_wasmi::Wasmi;

use crate::irq::SCHED;
use crate::kernel::HEAP;
use crate::memstat;

/// The booted system: `purg_system`'s, shared with the probe, with the
/// kernel's own checks on top.
pub struct System(purg_system::System);

impl core::ops::Deref for System {
    type Target = purg_system::System;
    fn deref(&self) -> &purg_system::System {
        &self.0
    }
}

#[cfg(feature = "net")]
pub use purg_system::with_test_keys;

/// Live kernel threads, for [`purg_system::System::reap`].
fn threads() -> usize {
    SCHED.live()
}

/// Builds the emu's boot namespace over a RAM root that `seed` fills,
/// with the kernel's `native` devices (bound after `/dev`) and the
/// devices `extra` makes (in the device table only, such as `#9`).
pub fn boot(
    platform: MetalPlatform,
    mut native: Vec<Arc<dyn Dev>>,
    extra: impl FnOnce(&Arc<dyn Platform>) -> (Vec<Arc<dyn Dev>>, Vec<Arc<dyn Dev>>),
    seed: impl FnOnce(&RamTree) -> Result<(), Error>,
) -> Result<System, Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform);
    let (more_native, extra) = extra(&platform);
    native.extend(more_native);
    let b = Boot {
        platform,
        native,
        extra,
        cmd: false,
    };
    let ns = purg_system::boot(b, |owner, platform| {
        let root = RamTree::new('U', "ramfs", owner, platform.clone());
        seed(&root)?;
        root.add_dir("/tmp");
        Ok(root)
    })?;
    let engine = Arc::new(Wasmi::new()?);
    memstat::report("booted");
    Ok(System(purg_system::System::new(engine, ns, threads)))
}

/// Commands run with the console as fds 0, 1 and 2, and whether each must
/// succeed. The probe runs the same list.
const SCRIPT: &[(&[&str], bool)] = &[
    (&["echo", "hello", "world"], true),
    (&["cat", "/lib/probe/hello.txt"], true),
    (&["cat", "/nonexistent"], false),
    (&["/bin/test/abi.wasm"], true),
    (&["/bin/test/procs.wasm"], true),
    (&["/bin/test/m3.wasm"], true),
    (&["/bin/test/net.wasm"], true),
    (&["/bin/test/wasi.wasm"], true),
];

/// Heap the conformance guests and the sh corpus (about 2000 progs) may
/// leave in use: files in `/tmp` and tables that grew. They leave about
/// 12 KiB; one prog left running holds over 3 MiB, and before each prog
/// had its own wasmi engine the run left over 330 MiB.
const LEAK_LIMIT: u64 = 1 << 20;

/// The `sh` scripts; each `x.sh` runs with its output compared with `x.out`.
const SH_CORPUS: &str = "/lib/test/sh";

/// The benchmark suite's script (`cargo xtask bench`, `docs/perf.md`).
const BENCH_SCRIPT: &str = "/lib/bench/bench.sh";

impl System {
    /// Prints `kernel: <v>` or `kernel: FAIL <v>`, counting a failure.
    pub(crate) fn verdict(&self, failed: &mut usize, v: core::result::Result<String, String>) {
        self.0.verdict("kernel", failed, v)
    }

    /// One line of heap figures and the number of live kernel threads,
    /// then the names and pids of the threads that have names. `run_peak`
    /// is the most the heap held since the line before; `peak` since boot.
    pub(crate) fn memstat(&self) {
        let h = HEAP.stats();
        let (run_peak, peak) = memstat::run_peak();
        self.say(&format!(
            "kernel: mem used={} run_peak={} peak={} largest={} size={} blocks={} allocs={} threads={}",
            h.used,
            run_peak,
            peak,
            h.largest_free,
            h.size,
            h.live,
            h.allocs,
            SCHED.live()
        ));
        let threads: Vec<String> = SCHED
            .threads()
            .iter()
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, pid)| format!("{name}:{pid}"))
            .collect();
        self.say(&format!("kernel: mem threads {}", threads.join(" ")));
    }

    /// Heap bytes in use and live kernel threads, once the stacks of
    /// finished threads are freed (which happens when another thread next
    /// runs).
    fn in_use(&self) -> (u64, usize) {
        use purg_platform::sync::Sched;
        for _ in 0..2 {
            SCHED.yield_now();
        }
        (HEAP.memory().0, SCHED.live())
    }

    /// Runs the conformance guests and the sh corpus (only the scripts
    /// named in `sh`, a comma-separated list, when given), then checks
    /// that they left no kernel thread and no more than [`LEAK_LIMIT`] of
    /// heap behind; returns the number that failed. With `memstat`,
    /// prints [`System::memstat`] after each.
    pub fn run_tests(&self, memstat: bool, sh: Option<&str>) -> usize {
        let mut failed = 0;
        let (heap, threads) = self.in_use();
        if memstat {
            self.memstat();
        }
        for &(argv, ok) in SCRIPT {
            self.say(&format!("kernel: run {}", argv.join(" ")));
            let v = match (self.run_test(argv, None).map(|(s, _)| s), ok) {
                (Ok(s), true) if s.is_empty() => Ok(String::from("ok")),
                (Ok(s), false) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
                (Ok(s), _) => Err(format!("status {s:?}")),
                (Err(e), _) => Err(format!("error: {e}")),
            };
            self.verdict(&mut failed, v);
            if memstat {
                self.memstat();
            }
        }
        failed += self.device_tests();
        let names = match self.list(SH_CORPUS) {
            Ok(n) => n,
            Err(e) => {
                self.say(&format!("kernel: FAIL {SH_CORPUS}: {e}"));
                return failed + 1;
            }
        };
        let wanted = |base: &str| sh.is_none_or(|l| l.split(',').any(|n| n == base));
        for base in names
            .iter()
            .filter_map(|n| n.strip_suffix(".sh"))
            .filter(|b| wanted(b))
        {
            let script = format!("{SH_CORPUS}/{base}.sh");
            self.say(&format!("kernel: sh {script}"));
            let want = self
                .read_file(&format!("{SH_CORPUS}/{base}.out"))
                .unwrap_or_default();
            let v = self
                .sh_script("kernel", &script, &want)
                .map(|()| String::from("ok"));
            self.verdict(&mut failed, v);
            if memstat {
                self.memstat();
            }
        }
        let (heap_now, threads_now) = self.in_use();
        let grew = heap_now.saturating_sub(heap);
        let v = if threads_now > threads {
            Err(format!(
                "the tests left {} kernel threads running",
                threads_now - threads
            ))
        } else if grew > LEAK_LIMIT {
            Err(format!("the tests left {grew} bytes of heap in use"))
        } else {
            Ok(String::from("the tests left no threads or memory behind"))
        };
        self.verdict(&mut failed, v);
        failed
    }

    /// The benchmark suite (`bootargs` has `bench`; `cargo xtask bench`):
    /// `sh /lib/bench/bench.sh` with its output on the console.
    pub fn bench(&self) -> usize {
        match self.run(&["sh", BENCH_SCRIPT], None) {
            Ok(s) if s.is_empty() => 0,
            Ok(s) => {
                self.say(&format!("kernel: bench: {s}"));
                1
            }
            Err(e) => {
                self.say(&format!("kernel: bench: {e}"));
                1
            }
        }
    }

    /// Boots like the emu: `emuinit` runs `sh -l` on the console.
    pub fn interactive(&self) -> Result<String, Error> {
        memstat::sampler();
        self.run(&["emuinit"], None)
    }
}

/// How the kernel starts the system.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Run the conformance guests and the sh corpus (`bootargs` has `test`).
    /// `memstat` (`bootargs` has `memstat` too) prints a line of heap
    /// figures after each. `sh=a,b` (`sh`) runs only those corpus
    /// scripts.
    Test {
        memstat: bool,
        sh: Option<&'static str>,
    },
    /// `sh -l` on the console.
    Interactive,
    /// The benchmark suite (`bootargs` has `bench`).
    Bench,
    /// The second boot of `xtask kernel` (`bootargs` has `diskcheck`):
    /// only check that the disk holds what the first boot left.
    DiskCheck,
}

/// Boots the namespace over `initrd` and runs the system in a kernel
/// thread (wasmi needs more stack than the boot stack has). The boot
/// thread sleeps until it is done; the result is the number of failures
/// in test mode, or 0 or 1 for the shell's exit status.
pub fn start(
    initrd: &'static [u8],
    console: &'static dyn Console,
    rtc: Option<usize>,
    (stack, checks): (net::Stack, net::Checks),
    host: Option<host9p::Host>,
    blks: disk::Blks,
    mode: Mode,
) -> usize {
    use core::sync::atomic::AtomicUsize;
    use purg_platform::sync::Rendez;
    static DONE: AtomicUsize = AtomicUsize::new(usize::MAX);
    let done = Rendez::new();
    let wake = done.clone();
    SCHED.spawn(Box::new(move || {
        let platform = MetalPlatform::new(console, &SCHED, "purgatorio", rtc)
            .with_heap(&HEAP)
            .with_kproc_stack(crate::irq::KPROC_STACK_SIZE);
        let user = platform.user();
        let native = crate::devices::native(console, rtc, &user);
        let p9 = host.and_then(host9p::open);
        let mut disks = None;
        let devs = |platform: &Arc<dyn Platform>| {
            let d = disk::open(blks, platform);
            let native = disk::devs(&d, &user);
            disks = Some(d);
            (native, host9p::devs(p9.as_ref()))
        };
        let booted = boot(net::attach(platform, stack), native, devs, |root| {
            root.add_tar(initrd).map(|_| ())
        });
        let disks = disks.unwrap_or_default();
        let result = match booted {
            Err(e) => crate::kernel::fail(format_args!("boot: {e}")),
            Ok(sys) => match (mode, sys.mount_host9p(p9.as_deref()), sys.bind_disk(&disks)) {
                (Mode::Test { memstat, sh }, host, bound) => {
                    let failed = sys.run_tests(memstat, sh)
                        + sys.host9p_tests(host, p9.as_deref())
                        + sys.disk_tests(&disks, bound)
                        + sys.net_tests(&checks);
                    if memstat {
                        sys.memstat();
                    }
                    failed
                }
                (Mode::DiskCheck, _, bound) => sys.disk_persist(&disks, bound),
                (Mode::Bench, _, _) => sys.bench(),
                (Mode::Interactive, _, _) => match sys.interactive() {
                    Ok(s) if s.is_empty() => 0,
                    Ok(s) => {
                        sys.say(&format!("kernel: emuinit: {s}"));
                        1
                    }
                    Err(e) => {
                        sys.say(&format!("kernel: emuinit: {e}"));
                        1
                    }
                },
            },
        };
        memstat::report("end");
        DONE.store(result, Ordering::Release);
        wake.wakeup();
    }));
    // Sleeping, rather than yielding in a loop, lets the CPU idle.
    done.sleep(|| DONE.load(Ordering::Acquire) != usize::MAX);
    DONE.load(Ordering::Acquire)
}
