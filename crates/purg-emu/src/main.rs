//! `emu`: Purgatorio hosted on Linux or macOS.
//!
//! Usage: `emu [-r root] [--vm=wasmtime|wasmi] [--modules=none|warn|require]
//! [cmd [args...]]`
//!
//! Boots a namespace like Inferno's emu (`#/` with the host root `#U` unioned
//! after it and taking creates, `#c` before `/dev`, `#e` on `/env` and `#p`
//! on `/prog`), then runs `/bin/<cmd>.wasm` as the first prog, or
//! `/bin/emuinit.wasm` when no command is given. The emu exits when the first
//! prog does: with 0 when its exit status is empty and 1 otherwise. Other
//! progs may still be running then, so the process ends without the C
//! library's exit handlers (`host::exit`).
//!
//! A panic is reported with its thread, its prog and a backtrace, whatever
//! `RUST_BACKTRACE` says, and an abort from C with its thread and prog
//! (`host::report_aborts`): the emu's tests run it with its standard error
//! captured, and a crash has to explain itself there.
//!
//! `--modules` sets the system's module policy (`docs/signing.md`) before
//! the first prog is loaded, with the trust lists read in the boot
//! namespace.

use std::backtrace::Backtrace;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_core::devroot::StaticTree;
use purg_core::modsig::Level;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_emu::{devfs, host, platform};
use purg_vm::{Ctx, Engine};

struct Options {
    root: PathBuf,
    vm: String,
    modules: Level,
    argv: Vec<String>,
}

fn usage() -> u8 {
    eprintln!(
        "usage: emu [-r root] [--vm=wasmtime|wasmi] [--modules=none|warn|require] [cmd [args...]]"
    );
    2
}

fn parse_args() -> Option<Options> {
    let mut args = std::env::args().skip(1);
    let mut opts = Options {
        root: std::env::var_os("PURGATORIO_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("root")),
        vm: String::from("wasmtime"),
        modules: Level::None,
        argv: Vec::new(),
    };
    while let Some(a) = args.next() {
        if a == "-r" {
            opts.root = PathBuf::from(args.next()?);
        } else if let Some(r) = a.strip_prefix("-r") {
            opts.root = PathBuf::from(r);
        } else if let Some(vm) = a.strip_prefix("--vm=") {
            opts.vm = String::from(vm);
        } else if let Some(level) = a.strip_prefix("--modules=") {
            opts.modules = Level::parse(level)?;
        } else if a == "--" {
            opts.argv.extend(args.by_ref());
        } else if a.starts_with('-') && a.len() > 1 {
            return None;
        } else {
            opts.argv.push(a);
            opts.argv.extend(args.by_ref());
        }
    }
    Some(opts)
}

/// Builds the boot namespace and the first prog.
fn boot(root: PathBuf, modules: Level) -> Result<Prog, Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform::Emu::new());
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root(procs.user()))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.push(Arc::new(devfs::DevFs::new(root)?));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(
        platform.clone(),
        devtab.clone(),
        procs,
        portable.srv,
        portable.mnt,
    );
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "")?);
    pgrp.bind("#U", "/", MAFTER | MCREATE)?;
    pgrp.bind("#c", "/dev", MBEFORE)?;
    pgrp.bind("#e", "/env", MREPL | MCREATE)?;
    pgrp.bind("#p", "/prog", MREPL)?;
    pgrp.bind("#s", "/chan", MREPL | MCREATE)?;
    pgrp.bind("#d", "/fd", MREPL)?;
    pgrp.bind("#I", "/net", MREPL)?;
    pgrp.bind("#C", "/cmd", MREPL)?;
    kernel.procs.set_module_policy(modules, &pgrp);
    let mut prog = Prog::new(kernel, pgrp);
    platform.set_current_pid(prog.pid);
    purg_vm::init_fds(&mut prog)?;
    Ok(prog)
}

fn engine(name: &str) -> Result<Arc<dyn Engine>, Error> {
    match name {
        "wasmtime" => Ok(Arc::new(purg_vm_wasmtime::Wasmtime::new()?)),
        "wasmi" => Ok(Arc::new(purg_vm_wasmi::Wasmi::new()?)),
        _ => Err(Error::from(format!("unknown vm {name:?}"))),
    }
}

/// Reports a panic in one write, so that panics on several threads do
/// not interleave: the thread, the prog it ran or worked for (0 for
/// none), the message and place, and a backtrace.
fn report_panics() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let report = format!(
            "emu: panic on thread '{}', prog {}: {info}\n{}\n",
            thread.name().unwrap_or("?"),
            platform::current_pid(),
            Backtrace::force_capture()
        );
        let _ = std::io::stderr().write_all(report.as_bytes());
    }));
}

fn main() {
    report_panics();
    host::report_aborts();
    host::exit(run());
}

/// Runs the emu; returns the process's exit status.
fn run() -> u8 {
    let Some(opts) = parse_args() else {
        return usage();
    };
    let (path, argv) = match opts.argv.first() {
        Some(cmd) => (purg_vm::command_path(cmd), opts.argv.clone()),
        None => (
            String::from("/bin/emuinit.wasm"),
            vec![String::from("emuinit")],
        ),
    };
    let setup = engine(&opts.vm).and_then(|e| Ok((e, boot(opts.root.clone(), opts.modules)?)));
    let (engine, prog) = match setup {
        Ok(v) => v,
        Err(e) => {
            eprintln!("emu: {e}");
            return 1;
        }
    };
    let ctx = Ctx::new(prog, argv, engine);
    let runner = std::thread::Builder::new()
        .name(String::from(purg_vm::module_name(&path)))
        .stack_size(platform::KPROC_STACK)
        .spawn(move || purg_vm::run_command(ctx, &path).1);
    let res = match runner.map(|t| t.join()) {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => Err(Error::new("prog thread panicked")),
        Err(e) => Err(Error::from(format!("no free procs: {e}"))),
    };
    match res {
        Ok(status) if status.is_empty() => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("emu: {e}");
            1
        }
    }
}
