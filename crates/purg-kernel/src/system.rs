//! The shared core on metal (K2): the kernel's `Platform` (a
//! `purg_aarch64::platform::MetalPlatform`), the boot
//! namespace, and the programs the kernel runs: the conformance guests and
//! the `sh` corpus in test mode, or `sh -l` on the console.
//!
//! The namespace is the emu's: `#/` is the root, `#U` (here a RAM tree
//! seeded from the initrd) is unioned after it and takes creates, `#c` is
//! before `/dev`, and `#e`, `#p`, `#s`, `#d` are on `/env`, `/prog`,
//! `/chan` and `/fd`. There is no host file system, no host commands and no
//! network yet (K3).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

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

/// The booted system: an engine, the kernel and the boot namespace.
pub struct System {
    engine: Arc<dyn Engine>,
    kernel: Arc<Kernel>,
    pgrp: Arc<Pgrp>,
}

/// Builds the emu's boot namespace over a RAM root that `seed` fills.
pub fn boot(
    platform: MetalPlatform,
    seed: impl FnOnce(&RamTree) -> Result<(), Error>,
) -> Result<System, Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform);
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root(procs.user()))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let root = RamTree::new('U', "ramfs", procs.user(), platform.clone());
    seed(&root)?;
    root.add_dir("/tmp");
    devs.push(Arc::new(root));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "")?);
    pgrp.bind("#U", "/", MAFTER | MCREATE)?;
    pgrp.bind("#c", "/dev", MBEFORE)?;
    pgrp.bind("#e", "/env", MREPL | MCREATE)?;
    pgrp.bind("#p", "/prog", MREPL)?;
    pgrp.bind("#s", "/chan", MREPL | MCREATE)?;
    pgrp.bind("#d", "/fd", MREPL)?;
    Ok(System {
        engine: Arc::new(Wasmi::new()?),
        kernel,
        pgrp,
    })
}

impl System {
    /// Runs `argv` in a fresh prog on this thread. Its fd 0 is the console,
    /// and fds 1 and 2 are the console or, given `out`, that file. Returns
    /// the prog's exit status.
    pub fn run(&self, argv: &[&str], out: Option<&str>) -> Result<String, Error> {
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
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
        purg_vm::run_command(Ctx::new(prog, argv, self.engine.clone()), &path).1
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
        let status = self
            .run(&["sh", script], Some(&out))
            .map_err(|e| format!("error: {e}"))?;
        let got = self.read_file(&out).map_err(|e| format!("{out}: {e}"))?;
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
/// succeed. The probe runs the same list plus the network check (K3).
const SCRIPT: &[(&[&str], bool)] = &[
    (&["echo", "hello", "world"], true),
    (&["cat", "/lib/probe/hello.txt"], true),
    (&["cat", "/nonexistent"], false),
    (&["/bin/test/abi.wasm"], true),
    (&["/bin/test/procs.wasm"], true),
    (&["/bin/test/m3.wasm"], true),
    (&["/bin/test/wasi.wasm"], true),
];

/// The `sh` scripts; each `x.sh` runs with its output compared with `x.out`.
const SH_CORPUS: &str = "/lib/test/sh";

impl System {
    fn say(&self, s: &str) {
        let c = self.kernel.platform.console();
        let _ = c.write(s.as_bytes());
        let _ = c.write(b"\n");
    }

    fn verdict(&self, failed: &mut usize, v: core::result::Result<String, String>) {
        match v {
            Ok(v) => self.say(&format!("kernel: {v}")),
            Err(v) => {
                self.say(&format!("kernel: FAIL {v}"));
                *failed += 1;
            }
        }
    }

    /// Runs the conformance guests and the sh corpus; returns the number
    /// that failed.
    pub fn run_tests(&self) -> usize {
        let mut failed = 0;
        for &(argv, ok) in SCRIPT {
            self.say(&format!("kernel: run {}", argv.join(" ")));
            let v = match (self.run(argv, None), ok) {
                (Ok(s), true) if s.is_empty() => Ok(String::from("ok")),
                (Ok(s), false) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
                (Ok(s), _) => Err(format!("status {s:?}")),
                (Err(e), _) => Err(format!("error: {e}")),
            };
            self.verdict(&mut failed, v);
        }
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
        }
        failed
    }

    /// Boots like the emu: `emuinit` runs `sh -l` on the console.
    pub fn interactive(&self) -> Result<String, Error> {
        self.run(&["emuinit"], None)
    }
}

/// How the kernel starts the system.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Run the conformance guests and the sh corpus (`bootargs` has `test`).
    Test,
    /// `sh -l` on the console.
    Interactive,
}

/// Boots the namespace over `initrd` and runs the system in a kernel
/// thread (wasmi needs more stack than the boot stack has). The boot
/// thread waits; the result is the number of failures in test mode, or 0
/// or 1 for the shell's exit status.
pub fn start(
    initrd: &'static [u8],
    console: &'static dyn Console,
    rtc: Option<usize>,
    mode: Mode,
) -> usize {
    use core::sync::atomic::AtomicUsize;
    use purg_platform::sync::Sched;
    static DONE: AtomicUsize = AtomicUsize::new(usize::MAX);
    SCHED.spawn(Box::new(move || {
        let result = match boot(
            MetalPlatform::new(console, &SCHED, "purgatorio", rtc),
            |root| root.add_tar(initrd).map(|_| ()),
        ) {
            Err(e) => crate::kernel::fail(format_args!("boot: {e}")),
            Ok(sys) => match mode {
                Mode::Test => sys.run_tests(),
                Mode::Interactive => match sys.interactive() {
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
        DONE.store(result, Ordering::Release);
    }));
    loop {
        let r = DONE.load(Ordering::Acquire);
        if r != usize::MAX {
            return r;
        }
        SCHED.yield_now();
    }
}
