//! The probe proper: heap, boot namespace and the script.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::fmt::Write;

use linked_list_allocator::LockedHeap;
use purg_abi::mode;
use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_vm::{Ctx, Engine};
use purg_vm_wasmi::Wasmi;

use crate::sched::SCHED;
use crate::{hw, platform};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

unsafe extern "C" {
    static mut __heap_start: u8;
    static mut __heap_end: u8;
}

/// How a script command must end.
enum Want {
    Ok,
    Fail,
}

/// Commands run with the console as fds 0, 1 and 2.
const SCRIPT: &[(&[&str], Want)] = &[
    (&["echo", "hello", "world"], Want::Ok),
    (&["cat", "/lib/probe/hello.txt"], Want::Ok),
    (&["cat", "/nonexistent"], Want::Fail),
    (&["/bin/test/abi.wasm"], Want::Ok),
    (&["/bin/test/procs.wasm"], Want::Ok),
    (&["/bin/test/m3.wasm"], Want::Ok),
    (&["/bin/test/net.wasm"], Want::Ok),
];

/// The sh scripts run with their output captured and compared with the
/// `.out` file beside each.
const SH_CORPUS: &str = "/lib/test/sh/";

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

fn embedded(path: &str) -> Option<&'static [u8]> {
    embedded::FILES.iter().find(|f| f.0 == path).map(|f| f.1)
}

/// The probe's `#U`: a RAM tree seeded with the embedded files, standing in
/// for the emu's host tree so the boot namespace is the same.
fn ram_root(owner: &str, platform: &Arc<dyn Platform>) -> RamTree {
    let t = RamTree::new('U', "ramfs", owner, platform.clone());
    for &(path, data) in embedded::FILES {
        let perm = if path.ends_with(".wasm") {
            0o755
        } else {
            0o644
        };
        t.add_file(path, data, perm);
    }
    t.add_dir("/tmp");
    t
}

/// Builds the emu's boot namespace.
fn boot() -> Result<(Arc<Kernel>, Arc<Pgrp>), Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform::Probe::new());
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root(procs.user()))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.push(Arc::new(ram_root(procs.user(), &platform)));
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
    pgrp.bind("#C", "/cmd", MREPL)?;
    Ok((kernel, pgrp))
}

struct System {
    engine: Arc<dyn Engine>,
    kernel: Arc<Kernel>,
    pgrp: Arc<Pgrp>,
}

impl System {
    /// Runs `argv` in a fresh prog on this thread. Its fd 0 is the console,
    /// and fds 1 and 2 are the console or, given `out`, that file.
    fn run(&self, argv: &[&str], out: Option<&str>) -> Result<String, Error> {
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

    fn read_file(&self, path: &str) -> Result<Vec<u8>, Error> {
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        let fd = prog.open(path, mode::OREAD)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = prog.read(fd, &mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Runs one corpus script; `Err` describes how it went wrong.
    fn sh_script(&self, script: &str, want: &[u8]) -> Result<(), String> {
        let name = script.trim_start_matches(SH_CORPUS);
        let out = format!("/tmp/probe-{name}.got");
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

/// Announces `tcp!*!6666` and serves `/` to the first connection. The host
/// forwards that port and mounts it; this returns when the client hangs up.
fn serve_export(sys: &System) -> Result<(), Error> {
    let mut prog = Prog::new(sys.kernel.clone(), sys.pgrp.clone());
    sys.kernel.platform.set_current_pid(prog.pid);
    let ctl = prog.open("/net/tcp/clone", mode::ORDWR)?;
    let mut buf = [0u8; 32];
    let n = prog.read(ctl, &mut buf)?;
    let id = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
    if id.is_empty() {
        return Err(Error::from("net: clone failed"));
    }
    prog.write(ctl, b"announce *!6666")?;
    println!("probe: net ready");
    let listen = prog.open(&format!("/net/tcp/{id}/listen"), mode::OREAD)?;
    buf = [0u8; 32];
    let n = prog.read(listen, &mut buf)?;
    let new_id = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
    if new_id.is_empty() {
        return Err(Error::from("net: listen failed"));
    }
    let data = prog.open(&format!("/net/tcp/{new_id}/data"), mode::ORDWR)?;
    let chan = prog.chan(data)?;
    purg_core::export::serve(
        sys.pgrp.clone(),
        "/",
        &chan,
        sys.kernel.platform.as_ref(),
        false,
    )?;
    println!("probe: net served");
    Ok(())
}

fn report(failed: &mut usize, verdict: Result<String, String>) {
    match verdict {
        Ok(v) => println!("probe: {v}"),
        Err(v) => {
            println!("probe: FAIL {v}");
            *failed += 1;
        }
    }
}

fn script() -> Result<usize, Error> {
    let (kernel, pgrp) = boot()?;
    let sys = System {
        engine: Arc::new(Wasmi::new()?),
        kernel,
        pgrp,
    };
    let mut failed = 0;
    for (argv, want) in SCRIPT {
        println!("probe: run {}", argv.join(" "));
        let verdict = match (sys.run(argv, None), want) {
            (Ok(s), Want::Ok) if s.is_empty() => Ok(String::from("ok")),
            (Ok(s), Want::Fail) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
            (Ok(s), _) => Err(format!("status {s:?}")),
            (Err(e), _) => Err(format!("error: {e}")),
        };
        report(&mut failed, verdict);
    }
    for &(path, _) in embedded::FILES {
        let Some(base) = path
            .strip_prefix(SH_CORPUS)
            .and_then(|p| p.strip_suffix(".sh"))
        else {
            continue;
        };
        println!("probe: sh {path}");
        let want = embedded(&format!("{SH_CORPUS}{base}.out")).unwrap_or_default();
        report(
            &mut failed,
            sys.sh_script(path, want).map(|()| String::from("ok")),
        );
    }
    if failed == 0
        && let Err(e) = serve_export(&sys)
    {
        println!("probe: net fail {e}");
        failed += 1;
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
    SCHED.init();
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
