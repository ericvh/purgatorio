//! The probe proper: heap, embedded root, boot namespace and the script.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::fmt::Write;

use linked_list_allocator::LockedHeap;
use purg_abi::mount::MBEFORE;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_vm::{Ctx, Engine};
use purg_vm_wasmi::Wasmi;

use crate::{hw, platform};

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

unsafe extern "C" {
    static mut __heap_start: u8;
    static mut __heap_end: u8;
}

macro_rules! embed {
    ($path:literal) => {
        include_bytes!(concat!(env!("PURG_PROBE_ROOT"), $path)).as_slice()
    };
}

/// How a script command must end.
enum Want {
    Ok,
    Fail,
}

/// The M1 portable subset.
const SCRIPT: &[(&[&str], Want)] = &[
    (&["echo", "hello", "world"], Want::Ok),
    (&["cat", "/lib/probe/hello.txt"], Want::Ok),
    (&["cat", "/nonexistent"], Want::Fail),
    (&["/bin/test/abi.wasm"], Want::Ok),
];

struct Uart;

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        hw::uart_write(s.as_bytes());
        Ok(())
    }
}

impl Uart {
    fn print(args: core::fmt::Arguments<'_>) {
        let _ = Uart.write_fmt(args);
    }
}

macro_rules! println {
    ($($t:tt)*) => {
        Uart::print(format_args!("{}\n", format_args!($($t)*)))
    };
}

/// The probe's `#/`: the emu's mount points plus the embedded files.
fn root_tree(owner: &str) -> StaticTree {
    let mut t = StaticTree::root(owner);
    t.add_file("/bin/echo.wasm", embed!("/bin/echo.wasm"));
    t.add_file("/bin/cat.wasm", embed!("/bin/cat.wasm"));
    t.add_file("/bin/test/abi.wasm", embed!("/bin/test/abi.wasm"));
    t.add_file("/lib/probe/hello.txt", embed!("/lib/probe/hello.txt"));
    t
}

fn boot() -> Result<(Arc<Kernel>, Arc<Pgrp>), Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform::Probe::new());
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(root_tree(procs.user()))];
    devs.extend(purg_core::portable_devices(&platform, &procs));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "")?);
    pgrp.bind("#c", "/dev", MBEFORE)?;
    Ok((kernel, pgrp))
}

/// Runs one command in a fresh prog sharing the boot namespace.
fn run(
    engine: &Arc<dyn Engine>,
    kernel: &Arc<Kernel>,
    pgrp: &Arc<Pgrp>,
    argv: &[&str],
) -> Result<String, Error> {
    let mut prog = Prog::new(kernel.clone(), pgrp.clone());
    kernel.platform.set_current_pid(prog.pid);
    purg_vm::init_fds(&mut prog)?;
    let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
    let path = purg_vm::command_path(&argv[0]);
    purg_vm::run_command(Ctx::new(prog, argv, engine.clone()), &path).1
}

fn script() -> Result<usize, Error> {
    let (kernel, pgrp) = boot()?;
    let engine: Arc<dyn Engine> = Arc::new(Wasmi::new()?);
    let mut failed = 0;
    for (argv, want) in SCRIPT {
        println!("probe: run {}", argv.join(" "));
        let verdict = match (run(&engine, &kernel, &pgrp, argv), want) {
            (Ok(s), Want::Ok) if s.is_empty() => Ok(String::from("ok")),
            (Ok(s), Want::Fail) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
            (Ok(s), _) => Err(format!("status {s:?}")),
            (Err(e), _) => Err(format!("error: {e}")),
        };
        match verdict {
            Ok(v) => println!("probe: {v}"),
            Err(v) => {
                println!("probe: FAIL {v}");
                failed += 1;
            }
        }
    }
    Ok(failed)
}

#[unsafe(no_mangle)]
extern "C" fn probe_main() -> ! {
    // SAFETY: the linker script reserves [__heap_start, __heap_end) for the
    // heap, and nothing else uses it.
    unsafe {
        let start = &raw mut __heap_start;
        let end = &raw mut __heap_end;
        HEAP.lock().init(start, end as usize - start as usize);
    }
    println!("purg-probe: aarch64 bare metal, wasmi");
    match script() {
        Ok(0) => {
            println!("probe: pass");
            hw::exit(0)
        }
        Ok(n) => {
            println!("probe: {n} failed");
            hw::exit(1)
        }
        Err(e) => {
            println!("probe: boot: {e}");
            hw::exit(1)
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn probe_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    println!("probe: exception {kind} esr={esr:#x} elr={elr:#x} far={far:#x}");
    hw::exit(3)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    println!("probe: panic: {info}");
    hw::exit(2)
}
