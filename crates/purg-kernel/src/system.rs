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
//! [`host9p`]) is mounted on `/n/host` when QEMU has one. There are no
//! host commands.

pub mod host9p;
pub mod net;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use purg_aarch64::net::NetStack;
use purg_aarch64::platform::MetalPlatform;
use purg_abi::mode;
use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_vm::{Ctx, Engine};
use purg_vm_wasmi::Wasmi;

use crate::irq::SCHED;
use crate::kernel::HEAP;
use crate::memstat;

/// The booted system: an engine, the kernel and the boot namespace.
pub struct System {
    engine: Arc<dyn Engine>,
    kernel: Arc<Kernel>,
    pgrp: Arc<Pgrp>,
}

/// Builds the emu's boot namespace over a RAM root that `seed` fills,
/// with the kernel's `native` devices (bound after `/dev`) and `extra`
/// devices (in the device table only, such as `#9`).
pub fn boot(
    platform: MetalPlatform,
    native: Vec<Arc<dyn Dev>>,
    extra: Vec<Arc<dyn Dev>>,
    seed: impl FnOnce(&RamTree) -> Result<(), Error>,
) -> Result<System, Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform);
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root(procs.user()))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.extend(extra);
    let root = RamTree::new('U', "ramfs", procs.user(), platform.clone());
    seed(&root)?;
    root.add_dir("/tmp");
    devs.push(Arc::new(root));
    let natives: Vec<char> = native.iter().map(|d| d.dc()).collect();
    devs.extend(native);
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "")?);
    pgrp.bind("#U", "/", MAFTER | MCREATE)?;
    pgrp.bind("#c", "/dev", MBEFORE)?;
    pgrp.bind("#e", "/env", MREPL | MCREATE)?;
    pgrp.bind("#p", "/prog", MREPL)?;
    pgrp.bind("#s", "/chan", MREPL | MCREATE)?;
    pgrp.bind("#d", "/fd", MREPL)?;
    pgrp.bind("#I", "/net", MREPL)?;
    for dc in natives {
        pgrp.bind(&format!("#{dc}"), "/dev", MAFTER)?;
    }
    let engine = Arc::new(Wasmi::new()?);
    memstat::report("booted");
    Ok(System {
        engine,
        kernel,
        pgrp,
    })
}

impl System {
    /// Runs `argv` in a fresh prog on this thread, in the boot namespace.
    /// Its fd 0 is the console, and fds 1 and 2 are the console or, given
    /// `out`, that file. Returns the prog's exit status, once every prog
    /// it started is gone (see [`System::reap`]).
    pub fn run(&self, argv: &[&str], out: Option<&str>) -> Result<String, Error> {
        let prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        let first = prog.pid;
        let status = self.run_prog(prog, argv, out);
        // A mount it made in the boot namespace keeps its reader thread.
        self.reap(first, usize::MAX)?;
        status
    }

    /// [`System::run`] in a copy of the boot namespace, as a test runs
    /// under its own emu when hosted: what it binds and mounts goes away
    /// with it, and so do the file servers behind those mounts. Returns
    /// the exit status and what `out` held when the prog exited: the progs
    /// it left behind may write there when they are killed.
    pub fn run_test(&self, argv: &[&str], out: Option<&str>) -> Result<(String, Vec<u8>), Error> {
        let threads = SCHED.live();
        let prog = Prog::new(self.kernel.clone(), Arc::new(self.pgrp.fork()));
        let first = prog.pid;
        let status = self.run_prog(prog, argv, out);
        let got = out.map(|o| self.read_file(o));
        self.reap(first, threads)?;
        if let Some(out) = out {
            let _ = Prog::new(self.kernel.clone(), self.pgrp.clone()).remove(out);
        }
        Ok((status?, got.transpose()?.unwrap_or_default()))
    }

    /// Kills the progs with pids from `first` on that are still running,
    /// and waits for them to go, and for the kernel threads to be no more
    /// than `threads`: the progs' own, and the readers of the mounts in
    /// their namespaces.
    ///
    /// A command may leave progs behind: a file server it mounted, a
    /// background job. Each holds a 2 MiB stack and its wasm memory, so
    /// over a test run they would add up.
    fn reap(&self, first: u32, threads: usize) -> Result<(), Error> {
        use purg_platform::sync::Rendez;
        const MS: u64 = 1_000_000;
        let r = Rendez::new();
        let procs = &self.kernel.procs;
        let left = || procs.pids().into_iter().filter(|&p| p >= first);
        for _ in 0..REAP_MS / 10 {
            if left().next().is_none() && SCHED.live() <= threads {
                return Ok(());
            }
            // Again each time round: a prog may have been spawned since.
            for p in left().filter_map(|p| procs.get(p)) {
                p.kill();
            }
            r.tsleep(|| false, 10 * MS);
        }
        let names: Vec<String> = left()
            .filter_map(|p| procs.get(p))
            .map(|p| format!("{} {}", p.pid, p.name()))
            .collect();
        Err(Error::from(format!(
            "{} progs ({}) and {} kernel threads would not die",
            names.len(),
            names.join(", "),
            SCHED.live().saturating_sub(threads)
        )))
    }

    fn run_prog(&self, mut prog: Prog, argv: &[&str], out: Option<&str>) -> Result<String, Error> {
        self.kernel.platform.set_current_pid(prog.pid);
        match out {
            None => purg_vm::init_fds(&mut prog)?,
            Some(out) => {
                prog.open("/dev/cons", mode::OREAD)?;
                prog.create(out, mode::OWRITE, 0o644)?;
                prog.dup(1, 2)?;
                purg_vm::init_preopens(&mut prog)?;
            }
        }
        let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
        let path = purg_vm::command_path(&argv[0]);
        let (ctx, status) = purg_vm::run_command(Ctx::new(prog, argv, self.engine.clone()), &path);
        purg_vm::exit(ctx);
        status
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, Error> {
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        let fd = prog.open(path, mode::OREAD)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = prog.read(fd, &mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// The names in directory `path`, sorted.
    pub fn list(&self, path: &str) -> Result<Vec<String>, Error> {
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        let fd = prog.open(path, mode::OREAD)?;
        let mut names = Vec::new();
        let mut buf = vec![0u8; 8192];
        loop {
            let n = prog.dirread(fd, &mut buf)?;
            if n == 0 {
                break;
            }
            let mut off = 0;
            while off < n {
                let Ok((d, used)) = purg_styx::Dir::unpack(&buf[off..n]) else {
                    return Err(Error::from("bad directory entry"));
                };
                names.push(String::from(d.name.as_str()));
                off += used;
            }
        }
        names.sort();
        Ok(names)
    }

    /// Runs one corpus script with its output in a file and compares it
    /// with `want`; `Err` says how it went wrong.
    pub fn sh_script(&self, script: &str, want: &[u8]) -> core::result::Result<(), String> {
        let name = script.rsplit('/').next().unwrap_or(script);
        let out = format!("/tmp/kernel-{name}.got");
        let (status, got) = self
            .run_test(&["sh", script], Some(&out))
            .map_err(|e| format!("error: {e}"))?;
        if got != want {
            return Err(format!(
                "output differs\n--- got\n{}--- want\n{}---",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(want)
            ));
        }
        if !status.is_empty() {
            return Err(format!("status {status:?}"));
        }
        Ok(())
    }
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

/// How long [`System::reap`] waits for killed progs to go.
const REAP_MS: u64 = 5000;

/// The `sh` scripts; each `x.sh` runs with its output compared with `x.out`.
const SH_CORPUS: &str = "/lib/test/sh";

impl System {
    pub(crate) fn say(&self, s: &str) {
        let c = self.kernel.platform.console();
        let _ = c.write(s.as_bytes());
        let _ = c.write(b"\n");
    }

    pub(crate) fn verdict(&self, failed: &mut usize, v: core::result::Result<String, String>) {
        match v {
            Ok(v) => self.say(&format!("kernel: {v}")),
            Err(v) => {
                self.say(&format!("kernel: FAIL {v}"));
                *failed += 1;
            }
        }
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

    /// Runs the conformance guests and the sh corpus, then checks that
    /// they left no kernel thread and no more than [`LEAK_LIMIT`] of heap
    /// behind; returns the number that failed. With `memstat`, prints
    /// [`System::memstat`] after each.
    pub fn run_tests(&self, memstat: bool) -> usize {
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
        for base in names.iter().filter_map(|n| n.strip_suffix(".sh")) {
            let script = format!("{SH_CORPUS}/{base}.sh");
            self.say(&format!("kernel: sh {script}"));
            let want = self
                .read_file(&format!("{SH_CORPUS}/{base}.out"))
                .unwrap_or_default();
            let v = self.sh_script(&script, &want).map(|()| String::from("ok"));
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
    /// figures after each.
    Test { memstat: bool },
    /// `sh -l` on the console.
    Interactive,
}

/// Boots the namespace over `initrd` and runs the system in a kernel
/// thread (wasmi needs more stack than the boot stack has). The boot
/// thread sleeps until it is done; the result is the number of failures
/// in test mode, or 0 or 1 for the shell's exit status.
pub fn start(
    initrd: &'static [u8],
    console: &'static dyn Console,
    rtc: Option<usize>,
    (stack, checks): (NetStack, net::Checks),
    host: Option<purg_aarch64::p9::Host9p>,
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
        let native = crate::devices::native(console, rtc, &platform.user());
        let p9 = host.and_then(host9p::open);
        let extra: Vec<Arc<dyn Dev>> = p9.iter().map(|d| d.clone() as Arc<dyn Dev>).collect();
        let result = match boot(platform.with_net(Box::new(stack)), native, extra, |root| {
            root.add_tar(initrd).map(|_| ())
        }) {
            Err(e) => crate::kernel::fail(format_args!("boot: {e}")),
            Ok(sys) => match (mode, sys.mount_host9p(p9.as_deref())) {
                (Mode::Test { memstat }, host) => {
                    let failed =
                        sys.run_tests(memstat) + sys.host9p_tests(host) + sys.net_tests(&checks);
                    if memstat {
                        sys.memstat();
                    }
                    failed
                }
                (Mode::Interactive, _) => match sys.interactive() {
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
