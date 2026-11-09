//! `emu`: Purgatorio hosted on Linux or macOS.
//!
//! Usage: `emu [-r root] [--vm=wasmtime|wasmi] [cmd [args...]]`
//!
//! Boots a namespace like Inferno's emu (`#/` with the host root `#U` unioned
//! after it, and `#c` before `/dev`), then runs `/bin/<cmd>.wasm` in one prog,
//! or `/bin/emuinit.wasm` when no command is given. The exit code is 0 when
//! the prog's exit status is empty and 1 otherwise.

mod devfs;
mod host;
mod platform;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use purg_abi::mount::{MAFTER, MBEFORE};
use purg_core::devcons::DevCons;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Prog};
use purg_vm::{Ctx, Engine};

/// Default stack for prog threads. macOS gives secondary threads only
/// 512 KiB, and the wasmi interpreter needs more in debug builds.
const PROG_STACK: usize = 64 << 20;

struct Options {
    root: PathBuf,
    vm: String,
    argv: Vec<String>,
}

fn usage() -> ExitCode {
    eprintln!("usage: emu [-r root] [--vm=wasmtime|wasmi] [cmd [args...]]");
    ExitCode::from(2)
}

fn parse_args() -> Option<Options> {
    let mut args = std::env::args().skip(1);
    let mut opts = Options {
        root: std::env::var_os("PURGATORIO_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("root")),
        vm: String::from("wasmtime"),
        argv: Vec::new(),
    };
    while let Some(a) = args.next() {
        if a == "-r" {
            opts.root = PathBuf::from(args.next()?);
        } else if let Some(r) = a.strip_prefix("-r") {
            opts.root = PathBuf::from(r);
        } else if let Some(vm) = a.strip_prefix("--vm=") {
            opts.vm = String::from(vm);
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
fn boot(root: PathBuf) -> Result<Prog, Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform::Emu::new());
    let user = platform.user();
    let devs: Vec<Arc<dyn Dev>> = vec![
        Arc::new(StaticTree::root(&user)),
        Arc::new(DevCons::new(platform.clone())),
        Arc::new(devfs::DevFs::new(root)?),
    ];
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone());
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "")?);
    pgrp.bind("#U", "/", MAFTER)?;
    pgrp.bind("#c", "/dev", MBEFORE)?;
    let mut prog = Prog::new(kernel, pgrp);
    purg_vm::init_fds(&mut prog)?;
    Ok(prog)
}

fn engine(name: &str) -> Result<Box<dyn Engine + Send>, Error> {
    match name {
        "wasmtime" => Ok(Box::new(purg_vm_wasmtime::Wasmtime::new()?)),
        "wasmi" => Ok(Box::new(purg_vm_wasmi::Wasmi::new()?)),
        _ => Err(Error::from(format!("unknown vm {name:?}"))),
    }
}

fn main() -> ExitCode {
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
    let setup = engine(&opts.vm).and_then(|e| Ok((e, boot(opts.root.clone())?)));
    let (engine, prog) = match setup {
        Ok(v) => v,
        Err(e) => {
            eprintln!("emu: {e}");
            return ExitCode::FAILURE;
        }
    };
    let ctx = Ctx::new(prog, argv);
    let runner = {
        let path = path.clone();
        std::thread::Builder::new()
            .name(String::from("prog"))
            .stack_size(PROG_STACK)
            .spawn(move || purg_vm::run_command(&*engine, ctx, &path).1)
    };
    let res = match runner.map(|t| t.join()) {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => Err(Error::new("prog thread panicked")),
        Err(e) => Err(Error::from(format!("no free procs: {e}"))),
    };
    match res {
        Ok(status) if status.is_empty() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("emu: {e}");
            ExitCode::FAILURE
        }
    }
}
