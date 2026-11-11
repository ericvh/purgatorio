//! The system the bare-metal images run, shared by `purg-kernel` and
//! `purg-probe` (#57): the emu's boot namespace over a RAM root, and the
//! ways to run programs in it (a command on the console, a test in a copy
//! of the namespace whose leftovers are killed, a background prog) and to
//! compare their output with a golden file.
//!
//! It sits on `purg-core` and `purg-vm`, below the boards: the engine is
//! any [`Engine`], and the one thing it needs from the scheduler, the
//! number of live kernel threads, is a function the image passes in.
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use purg_abi::mode;
use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::sync::Rendez;
use purg_vm::{Ctx, Engine};

/// How long [`System::reap`] waits for killed progs to go, in ms.
pub const REAP_MS: u64 = 5000;

/// What [`boot`] builds the namespace from.
pub struct Boot {
    pub platform: Arc<dyn Platform>,
    /// Devices bound after `/dev`, as Inferno's native kernels bind theirs.
    pub native: Vec<Arc<dyn Dev>>,
    /// Devices in the device table only (reached by `#x` names).
    pub extra: Vec<Arc<dyn Dev>>,
    /// Bind `#C` on `/cmd`, as the emu does.
    pub cmd: bool,
}

/// Builds the emu's boot namespace: `#/` the root, the RAM tree made by
/// `root` (as `#U`, with its owner and the platform) unioned after it and
/// taking creates, `#c` before `/dev`, and `#e`, `#p`, `#s`, `#d` and `#I`
/// on `/env`, `/prog`, `/chan`, `/fd` and `/net`.
pub fn boot(
    b: Boot,
    root: impl FnOnce(&str, &Arc<dyn Platform>) -> Result<RamTree, Error>,
) -> Result<(Arc<Kernel>, Arc<Pgrp>), Error> {
    let platform = b.platform;
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root(procs.user()))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.extend(b.extra);
    devs.push(Arc::new(root(procs.user(), &platform)?));
    let natives: Vec<char> = b.native.iter().map(|d| d.dc()).collect();
    devs.extend(b.native);
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
    if b.cmd {
        pgrp.bind("#C", "/cmd", MREPL)?;
    }
    for dc in natives {
        pgrp.bind(&format!("#{dc}"), "/dev", MAFTER)?;
    }
    Ok((kernel, pgrp))
}

/// A copy of `pgrp` with the test keys (`/lib/test/keys`: the public
/// test key, for any user and any host) bound on `/lib/keys`, for the
/// servers the test modes start for the host. Nothing else gets it: the
/// boot namespace trusts no key (`docs/auth.md`, "Trust by default").
pub fn with_test_keys(pgrp: &Pgrp) -> Result<Arc<Pgrp>, Error> {
    let p = Arc::new(pgrp.fork());
    p.bind("/lib/test/keys", "/lib/keys", MREPL)?;
    Ok(p)
}

/// The booted system: an engine, the kernel and the boot namespace.
/// Clones share all three, so several threads can run progs in it.
#[derive(Clone)]
pub struct System {
    pub engine: Arc<dyn Engine>,
    pub kernel: Arc<Kernel>,
    pub pgrp: Arc<Pgrp>,
    /// The number of live kernel threads.
    threads: fn() -> usize,
}

impl System {
    pub fn new(
        engine: Arc<dyn Engine>,
        (kernel, pgrp): (Arc<Kernel>, Arc<Pgrp>),
        threads: fn() -> usize,
    ) -> System {
        System {
            engine,
            kernel,
            pgrp,
            threads,
        }
    }

    /// Live kernel threads.
    pub fn threads(&self) -> usize {
        (self.threads)()
    }

    /// Writes a line on the console.
    pub fn say(&self, s: &str) {
        let c = self.kernel.platform.console();
        let _ = c.write(s.as_bytes());
        let _ = c.write(b"\n");
    }

    /// Prints `v` after `prefix` (`kernel: ok`, `kernel: FAIL ...`) and
    /// counts a failure.
    pub fn verdict(&self, prefix: &str, failed: &mut usize, v: Result<String, String>) {
        match v {
            Ok(v) => self.say(&format!("{prefix}: {v}")),
            Err(v) => {
                self.say(&format!("{prefix}: FAIL {v}"));
                *failed += 1;
            }
        }
    }

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
        let threads = self.threads();
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

    /// [`System::run_test`] with `stdin` open for reading and writing as
    /// fd 0 and the console as fds 1 and 2, as `listen` starts a server on
    /// a connection. Its namespace trusts the test key, as the host's
    /// client proves with it ([`with_test_keys`]).
    pub fn run_test_on(&self, argv: &[&str], stdin: &str) -> Result<String, Error> {
        let threads = self.threads();
        let mut prog = Prog::new(self.kernel.clone(), with_test_keys(&self.pgrp)?);
        let first = prog.pid;
        self.kernel.platform.set_current_pid(prog.pid);
        let opened = prog
            .open(stdin, mode::ORDWR)
            .and_then(|_| prog.open("/dev/cons", mode::OWRITE))
            .and_then(|_| prog.dup(1, 2))
            .and_then(|_| purg_vm::init_preopens(&mut prog));
        let status = opened.and_then(|()| {
            let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
            let path = purg_vm::command_path(&argv[0]);
            let (ctx, status) =
                purg_vm::run_command(Ctx::new(prog, argv, self.engine.clone()), &path);
            purg_vm::exit(ctx);
            status
        });
        self.reap(first, threads)?;
        status
    }

    /// Starts `argv` in a new prog on its own kernel thread, in a copy of
    /// the boot namespace, with the console as fd 0 and `out` as fds 1
    /// and 2, and returns its pid without waiting: for the tests that kill
    /// a prog while it waits.
    pub fn spawn(&self, argv: &[&str], out: &str) -> Result<u32, Error> {
        let mut parent = Prog::new(self.kernel.clone(), Arc::new(self.pgrp.fork()));
        self.kernel.platform.set_current_pid(parent.pid);
        parent.open("/dev/cons", mode::OREAD)?;
        parent.open(out, mode::OWRITE)?;
        parent.dup(1, 2)?;
        purg_vm::init_preopens(&mut parent)?;
        let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
        let path = purg_vm::command_path(&argv[0]);
        purg_vm::spawn(&parent, &self.engine, &path, argv, 0, &[])
    }

    /// Kills the progs with pids from `first` on that are still running,
    /// removes those that are broken, and waits for them to go, and for
    /// the kernel threads to be no more than `threads`: the progs' own,
    /// and the readers of the mounts in their namespaces.
    ///
    /// A command may leave progs behind: a file server it mounted, a
    /// background job. Each holds a stack and its wasm memory, so over a
    /// test run they would add up.
    pub fn reap(&self, first: u32, threads: usize) -> Result<(), Error> {
        const MS: u64 = 1_000_000;
        let r = Rendez::new();
        let procs = &self.kernel.procs;
        let left = || procs.pids().into_iter().filter(|&p| p >= first);
        for _ in 0..REAP_MS / 10 {
            if left().next().is_none() && self.threads() <= threads {
                return Ok(());
            }
            // Again each time round: a prog may have been spawned since.
            // A broken prog has no thread to end: the kill removes it.
            for p in left() {
                procs.kill(p);
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
            self.threads().saturating_sub(threads)
        )))
    }

    /// [`System::run_test`] for runs that may overlap: `argv` runs in a
    /// copy of the boot namespace with the directory `tmp` (a boot
    /// namespace name) bound on `/tmp`, its fds 1 and 2 on `/tmp/cons`
    /// there, and in a prog domain of its own, whose `#pdomain` view is
    /// its `/prog`. So it sees only its own progs, and everything it
    /// starts, whatever its process group, can be found afterwards and
    /// killed without touching another run's. Returns the exit status and
    /// what it wrote. The file is named `cons` because `abi.wasm` checks
    /// that its stdout is.
    pub fn run_isolated(&self, argv: &[&str], tmp: &str) -> Result<(String, Vec<u8>), Error> {
        let ns = Arc::new(self.pgrp.fork());
        ns.bind(tmp, "/tmp", MREPL | MCREATE)?;
        let prog = Prog::new(self.kernel.clone(), ns.clone());
        // Attaching `#pdomain` as the new prog founds a domain named by
        // its process group, which is its pid.
        let domain = prog.proc.group();
        self.kernel.platform.set_current_pid(prog.pid);
        ns.bind("#pdomain", "/prog", MREPL)?;
        let status = self.run_prog(prog, argv, Some("/tmp/cons"));
        let out = format!("{tmp}/cons");
        let got = self.read_file(&out);
        self.reap_domain(domain)?;
        let _ = Prog::new(self.kernel.clone(), self.pgrp.clone()).remove(&out);
        Ok((status?, got?))
    }

    /// Kills the progs in domain `id` and waits for them to go.
    pub fn reap_domain(&self, id: u32) -> Result<(), Error> {
        const MS: u64 = 1_000_000;
        let r = Rendez::new();
        let procs = &self.kernel.procs;
        let left = || {
            procs
                .pids()
                .into_iter()
                .filter(|&p| procs.get(p).is_some_and(|p| p.in_domain(id)))
        };
        for _ in 0..REAP_MS / 10 {
            if left().next().is_none() {
                return Ok(());
            }
            for p in left() {
                procs.kill(p);
            }
            r.tsleep(|| false, 10 * MS);
        }
        let names: Vec<String> = left()
            .filter_map(|p| procs.get(p))
            .map(|p| format!("{} {}", p.pid, p.name()))
            .collect();
        Err(Error::from(format!(
            "{} progs ({}) would not die",
            names.len(),
            names.join(", ")
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

    /// Runs one corpus script as a test ([`System::run_test`]) with its
    /// output in `/tmp/<who>-<name>.got`, and compares it with `want`;
    /// `Err` says how it went wrong.
    pub fn sh_script(&self, who: &str, script: &str, want: &[u8]) -> Result<(), String> {
        let name = script.rsplit('/').next().unwrap_or(script);
        let out = format!("/tmp/{who}-{name}.got");
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

    /// Runs each command of `script` with the console as fds 0, 1 and 2,
    /// printing `<prefix>: run ...` and the verdict: each must succeed
    /// with an empty status, or fail if its flag is false. With `test`,
    /// each runs as [`System::run_test`]. Returns the number that went
    /// wrong.
    pub fn run_script(&self, prefix: &str, script: &[(&[&str], bool)], test: bool) -> usize {
        let mut failed = 0;
        for &(argv, ok) in script {
            self.say(&format!("{prefix}: run {}", argv.join(" ")));
            let status = if test {
                self.run_test(argv, None).map(|(s, _)| s)
            } else {
                self.run(argv, None)
            };
            let v = match (status, ok) {
                (Ok(s), true) if s.is_empty() => Ok(String::from("ok")),
                (Ok(s), false) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
                (Ok(s), _) => Err(format!("status {s:?}")),
                (Err(e), _) => Err(format!("error: {e}")),
            };
            self.verdict(prefix, &mut failed, v);
        }
        failed
    }
}
