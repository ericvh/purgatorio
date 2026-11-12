//! A prog killed while its engine is still starting the module ends with
//! the status `killed`, not with the engine's error for the interrupted
//! start (seen as `/bin/sleep.wasm: bad executable: exit "killed"`).

use std::sync::Arc;

use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_vm::{Ctx, Engine, KILLED, Stop, run_module};

struct NoConsole;

impl Console for NoConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

struct Mock(NoConsole);

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        0
    }
    fn monotonic_ns(&self) -> u64 {
        0
    }
    fn entropy(&self, buf: &mut [u8]) {
        buf.fill(1);
    }
    fn console(&self) -> &dyn Console {
        &self.0
    }
    fn user(&self) -> String {
        "inferno".into()
    }
    fn sysname(&self) -> String {
        "purgatorio".into()
    }
    fn current_pid(&self) -> u32 {
        0
    }
    fn set_current_pid(&self, _pid: u32) {}
    fn spawn_kproc(&self, _name: &str, _f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        panic!("no kproc here");
    }
}

/// An engine that is killed while it starts the module: it kills the prog
/// and reports the interrupted start as an error, as wasmtime and wasmi do
/// when a kill lands during instantiation.
struct KilledWhileStarting;

impl Engine for KilledWhileStarting {
    fn name(&self) -> &'static str {
        "killed while starting"
    }
    fn run(&self, _wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        ctx.prog.proc.kill();
        (ctx, Err(Error::from("bad executable: exit \"killed\"")))
    }
}

/// An engine that fails to start a module nobody killed.
struct BadModule;

impl Engine for BadModule {
    fn name(&self) -> &'static str {
        "bad module"
    }
    fn run(&self, _wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        (ctx, Err(Error::from("bad executable: no entry")))
    }
}

fn ctx(engine: Arc<dyn Engine>) -> Ctx {
    let platform: Arc<dyn Platform> = Arc::new(Mock(NoConsole));
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    Ctx::new(Prog::new(kernel, pgrp), vec!["sleep".into()], engine)
}

#[test]
fn a_kill_during_the_start_is_a_kill() {
    let (_ctx, res) = run_module(
        ctx(Arc::new(KilledWhileStarting)),
        vec![],
        "/bin/sleep.wasm",
    );
    assert_eq!(res.unwrap(), KILLED);
}

#[test]
fn a_bad_module_nobody_killed_is_still_an_error() {
    let (_ctx, res) = run_module(ctx(Arc::new(BadModule)), vec![], "/bin/bad.wasm");
    assert_eq!(
        res.unwrap_err().to_string(),
        "/bin/bad.wasm: bad executable: no entry"
    );
}
