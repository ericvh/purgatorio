//! The probe proper: bring-up, the boot namespace and the script.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use purg_aarch64::board::{board, exit_method, heap_region};
use purg_aarch64::boot::{self, DtbError, find_dtb};
use purg_aarch64::console::{self, Input};
use purg_aarch64::dtb::{BootInfo, Fdt};
use purg_aarch64::exception::{self, TrapFrame, Vector};
use purg_aarch64::exit;
use purg_aarch64::heap::Heap;
use purg_aarch64::irq;
use purg_aarch64::mmu::{self, Region};
use purg_aarch64::net::{Ipv4Config, NetStack};
use purg_aarch64::platform::MetalPlatform;
use purg_aarch64::sched::Scheduler;
use purg_abi::mode;
use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_platform::sync::Rendez;
use purg_vm::{Ctx, Engine};
use purg_vm_wasmi::Wasmi;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

#[global_allocator]
static HEAP: Heap = Heap::empty();

/// Stack for each kernel thread, sized as in the kernel (see
/// `purg-kernel`'s `irq.rs`): wasmi keeps the guest's stacks on the heap,
/// and the deepest a thread went there was 22 KiB.
const STACK_SIZE: usize = 256 << 10;

/// Stack for the core's service kprocs (the mount readers).
const KPROC_STACK_SIZE: usize = 64 << 10;

static SCHED: Scheduler = Scheduler::new(STACK_SIZE);

/// Timer interrupts per second; the tick preempts.
const TICK_HZ: u64 = 100;

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
    (&["/bin/test/wasi.wasm"], Want::Ok),
];

/// The sh scripts run with their output captured and compared with the
/// `.out` file beside each.
const SH_CORPUS: &str = "/lib/test/sh/";

/// Prints a line through the console lock.
macro_rules! println {
    ($($t:tt)*) => {
        console::print(format_args!("{}\n", format_args!($($t)*)))
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
fn boot(platform: MetalPlatform) -> Result<(Arc<Kernel>, Arc<Pgrp>), Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform);
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
        &sys.kernel.platform,
        false,
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

fn script(platform: MetalPlatform) -> Result<usize, Error> {
    let (kernel, pgrp) = boot(platform)?;
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

/// Exits with `code` after a final line, which is the verdict under PSCI.
/// The line is written unlocked: this may run in an exception handler, or
/// with the console lock held.
fn finish(code: u32, line: core::fmt::Arguments<'_>) -> ! {
    let _ = writeln!(console::Raw, "{line}");
    exit::exit(code)
}

fn sync(frame: &mut TrapFrame, esr: u64, far: u64) {
    finish(
        3,
        format_args!(
            "probe: exception esr={esr:#x} elr={:#x} far={far:#x}",
            frame.elr
        ),
    )
}

fn fatal(v: Vector, frame: &mut TrapFrame, esr: u64, far: u64) -> ! {
    finish(
        3,
        format_args!(
            "probe: exception {:?}/{:?} esr={esr:#x} elr={:#x} far={far:#x}",
            v.source, v.class, frame.elr
        ),
    )
}

/// Called by `purg_aarch64::boot` in the high half with the MMU on.
/// `dtb_phys` is x0 at boot: the DTB's physical address, or 0.
#[unsafe(no_mangle)]
extern "C" fn purg_main(dtb_phys: usize) -> ! {
    let dtb = match find_dtb(dtb_phys) {
        Ok((d, _)) => d,
        Err(DtbError::NotFound(pa)) => finish(1, format_args!("probe: no DTB at {pa:#x}")),
        Err(DtbError::OutsideRam(pa)) => {
            finish(1, format_args!("probe: DTB at {pa:#x} is outside RAM"))
        }
    };
    let (fdt, info) = match Fdt::new(dtb).and_then(|f| Ok((f, BootInfo::from_fdt(&f)?))) {
        Ok(x) => x,
        Err(e) => finish(1, format_args!("probe: DTB: {e:?}")),
    };
    exit::set_method(exit_method(&fdt, &info));
    if info.uart.base as usize != board().uart {
        finish(1, format_args!("probe: the DTB's PL011 is not the board's"));
    }
    let dtb_region = Region {
        base: mmu::virt_to_phys(dtb.as_ptr() as usize),
        size: dtb.len(),
    };
    // SAFETY: RAM after the image, clear of the DTB, is unused and
    // linear-mapped RW; this runs once, before any allocation.
    unsafe { HEAP.init(&[heap_region(board().ram, boot::image_end(), &[dtb_region])]) };
    exception::set_sync_handler(sync);
    exception::set_fatal_handler(fatal);
    irq::init(&info, &SCHED, TICK_HZ);
    // The probe's programs take no console input: an empty FIFO reads as
    // end of input.
    let cons: &'static dyn Console = console::init(&info, Input::Polled);
    println!("purg-probe: aarch64 bare metal, wasmi");
    let net = NetStack::open(&fdt, &Ipv4Config::QEMU_USER);
    let platform = MetalPlatform::new(
        cons,
        &SCHED,
        "purg-probe",
        info.rtc.map(|r| r.base as usize),
    )
    .with_heap(&HEAP)
    .with_kproc_stack(KPROC_STACK_SIZE)
    .with_net(Box::new(net));

    // The script runs in a kernel thread (the boot stack is small); the
    // boot thread sleeps until it is done.
    static RESULT: AtomicUsize = AtomicUsize::new(usize::MAX);
    let done = Rendez::new();
    let wake = done.clone();
    SCHED.spawn(Box::new(move || {
        let r = match script(platform) {
            Ok(n) => n,
            Err(e) => {
                println!("probe: boot: {e}");
                usize::MAX - 1
            }
        };
        RESULT.store(r, Ordering::Release);
        wake.wakeup();
    }));
    done.sleep(|| RESULT.load(Ordering::Acquire) != usize::MAX);
    match RESULT.load(Ordering::Acquire) {
        0 => finish(0, format_args!("probe: pass")),
        n if n == usize::MAX - 1 => exit::exit(1),
        n => finish(1, format_args!("probe: {n} failed")),
    }
}

/// Called by the early vectors on an exception before `irq::init`.
#[unsafe(no_mangle)]
extern "C" fn purg_early_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    finish(
        3,
        format_args!("probe: exception {kind} esr={esr:#x} elr={elr:#x} far={far:#x}"),
    )
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    finish(2, format_args!("probe: panic: {info}"))
}
