//! A mock platform and a prog on it, for the tests added with issue #69.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;

struct MockConsole;

impl Console for MockConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

pub struct Mock {
    console: MockConsole,
    /// Kprocs started and not yet finished.
    pub kprocs: Arc<AtomicUsize>,
}

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
        &self.console
    }
    fn user(&self) -> String {
        "inferno".into()
    }
    fn sysname(&self) -> String {
        "purgatorio".into()
    }
    fn current_pid(&self) -> u32 {
        PID.with(|c| c.get())
    }
    fn set_current_pid(&self, pid: u32) {
        PID.with(|c| c.set(pid));
    }
    fn spawn_kproc(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        let live = self.kprocs.clone();
        live.fetch_add(1, Ordering::AcqRel);
        thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                f();
                live.fetch_sub(1, Ordering::AcqRel);
            })
            .map(drop)
            .map_err(|_| purg_abi::error::ENOPROC.into())
    }
}

thread_local! {
    static PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A prog whose namespace has `#R` (a ram tree with `/mnt` and `/box`),
/// running on the calling thread.
pub fn setup() -> (Prog, Arc<Mock>) {
    let mock = Arc::new(Mock {
        console: MockConsole,
        kprocs: Arc::new(AtomicUsize::new(0)),
    });
    let platform: Arc<dyn Platform> = mock.clone();
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let ram = RamTree::new('R', "ram", "inferno", platform.clone());
    ram.add_dir("/mnt");
    ram.add_dir("/box");
    devs.push(Arc::new(ram));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let p = Prog::new(kernel, pgrp);
    enter(&mock, &p);
    (p, mock)
}

/// Makes `p` the prog running on the calling thread.
pub fn enter(mock: &Mock, p: &Prog) {
    mock.set_current_pid(p.pid);
    purg_platform::sync::set_current_note(Some(p.proc.note.clone()));
}
