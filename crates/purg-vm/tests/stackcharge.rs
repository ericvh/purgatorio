//! A spawn charges the child's kproc stack, where the platform allocates
//! one ([`Platform::prog_stack`]), to the child's memory limit, which it
//! inherits: past the limit the spawn fails with `out of memory: main`.

use std::sync::Arc;

use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_vm::{Ctx, Engine, Stop};

const STACK: usize = 256 << 10;

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
    fn prog_stack(&self) -> usize {
        STACK
    }
}

struct NoEngine;

impl Engine for NoEngine {
    fn name(&self) -> &'static str {
        "none"
    }
    fn run(&self, _wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        (ctx, Err(Error::from("not run")))
    }
}

fn prog() -> Prog {
    let platform: Arc<dyn Platform> = Arc::new(Mock(NoConsole));
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    Prog::new(kernel, pgrp)
}

#[test]
fn a_spawn_past_the_limit_is_out_of_memory() {
    let engine: Arc<dyn Engine> = Arc::new(NoEngine);
    let parent = prog();
    parent.proc.set_memlimit(STACK as u64 / 2).unwrap();
    let e = purg_vm::spawn(&parent, &engine, "/bin/x.wasm", vec!["x".into()], 0, &[]).unwrap_err();
    assert_eq!(e.to_string(), purg_abi::error::ENOMEM);
}

#[test]
fn a_spawn_within_the_limit_goes_on_to_load_the_module() {
    let engine: Arc<dyn Engine> = Arc::new(NoEngine);
    let parent = prog();
    parent.proc.set_memlimit(2 * STACK as u64).unwrap();
    // Charged, the spawn gets as far as the module, which is not there.
    let e = purg_vm::spawn(&parent, &engine, "/bin/x.wasm", vec!["x".into()], 0, &[]).unwrap_err();
    assert_eq!(e.to_string(), "/bin/x.wasm: file does not exist");
}
