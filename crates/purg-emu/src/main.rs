//! `emu`: Purgatorio hosted on Linux or macOS.
//!
//! Usage: `emu [-r root] [-g[WxH]] [-1|-2] [-w [addr]] [-s WxH] [-x] [--vm=wasmtime|wasmi]
//! [--modules=none|warn|require] [cmd [args...]]`
//!
//! Boots a namespace like Inferno's emu (`#/` with the host root `#U` unioned
//! after it and taking creates, `#c` before `/dev`, `#e` on `/env` and `#p`
//! on `/prog`), then runs `/bin/<cmd>.wasm` as the first prog, or
//! `/bin/emuinit.wasm` when no command is given. As in Inferno's emu
//! (`progexit` in emu/port/dis.c), the emu exits when the last prog does,
//! with status 0. A broken prog stays in the prog table until it is
//! killed, so it keeps the emu running.
//!
//! `-x` is not Inferno's: the emu exits when the first prog does, with 0
//! when its exit status is empty and 1 otherwise, whatever the other progs
//! are doing. Tests and scripts on the host use it to get a command's
//! status. Other progs may still be running then, so the process ends
//! without the C library's exit handlers (`host::exit`).
//!
//! A panic is reported with its thread, its prog and a backtrace, whatever
//! `RUST_BACKTRACE` says, and an abort from C with its thread and prog
//! (`host::report_aborts`): the emu's tests run it with its standard error
//! captured, and a crash has to explain itself there.
//!
//! `-g` opens a window of `W` by `H` pixels (800x600 by default) and
//! shows the draw device's screen in it (docs/graphics.md); the main
//! thread then runs the window's event loop, and closing the window ends
//! the emu and the progs still running, as in Inferno. Resizing the window resizes the screen. `-2` shows each
//! screen pixel as 2×2 window pixels and `-1` never does; by default the
//! emu doubles on a display whose scale factor is 1.5 or more. `-w`
//! serves the screen to a web browser instead (docs/graphics.md,
//! Section 8.2): the emu listens on `addr` (`127.0.0.1` and a free port
//! unless given; only loopback by default) and prints the URL to open,
//! which carries a random token. Without `-g` or `-w` the screen is in
//! memory only. `-s WxH` sets the screen's size in any of the three; it
//! is 800x600 with `-g` or `-w` and 640x480 in memory.
//!
//! `--modules` sets the system's module policy (`docs/signing.md`) before
//! the first prog is loaded, with the trust lists read in the boot
//! namespace.

use std::backtrace::Backtrace;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_core::devroot::StaticTree;
use purg_core::modsig::Level;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_emu::{devfs, host, platform, web, window};
use purg_platform::screen::{Headless, Screen};
use purg_vm::{Ctx, Engine};

struct Options {
    root: PathBuf,
    vm: String,
    modules: Level,
    /// `-g`: show the screen in a window.
    graphics: bool,
    /// Window pixels per screen pixel (`-1`, `-2`); automatic if `None`.
    scale: Option<u32>,
    /// `-w`: serve the screen to a browser on this address.
    web: Option<String>,
    /// The screen's size, from `-s` or `-g WxH`.
    size: Option<(u32, u32)>,
    /// `-x`: exit when the first prog does, with its status.
    first: bool,
    argv: Vec<String>,
}

/// The screen's size with `-g` or `-w` when none is given.
const WINDOW: (u32, u32) = (800, 600);

/// Parses `WxH`.
fn geometry(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0 && w <= 16384 && h <= 16384).then_some((w, h))
}

fn usage() -> u8 {
    eprintln!(
        "usage: emu [-r root] [-g[WxH]] [-1|-2] [-w [addr]] [-s WxH] [-x] [--vm=wasmtime|wasmi] [--modules=none|warn|require] [cmd [args...]]"
    );
    2
}

fn parse_args() -> Option<Options> {
    let args = std::env::args().skip(1);
    let mut opts = Options {
        root: std::env::var_os("PURGATORIO_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("root")),
        vm: String::from("wasmtime"),
        modules: Level::None,
        graphics: false,
        scale: None,
        web: None,
        size: None,
        first: false,
        argv: Vec::new(),
    };
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        if a == "-r" {
            opts.root = PathBuf::from(args.next()?);
        } else if let Some(r) = a.strip_prefix("-r") {
            opts.root = PathBuf::from(r);
        } else if a == "-g" {
            // An optional geometry as the next argument.
            let g = args.peek().and_then(|n| geometry(n));
            if g.is_some() {
                args.next();
            }
            opts.graphics = true;
            opts.size = g.or(opts.size);
        } else if a == "-1" || a == "-2" {
            opts.scale = Some(if a == "-2" { 2 } else { 1 });
        } else if let Some(g) = a.strip_prefix("-g") {
            opts.graphics = true;
            opts.size = Some(geometry(g)?);
        } else if a == "-w" {
            // An optional address as the next argument.
            let addr = args.peek().filter(|n| n.parse::<SocketAddr>().is_ok());
            opts.web = Some(addr.cloned().unwrap_or_else(|| web::DEFAULT_ADDR.into()));
            if addr.is_some() {
                args.next();
            }
        } else if let Some(addr) = a.strip_prefix("-w") {
            addr.parse::<SocketAddr>().ok()?;
            opts.web = Some(String::from(addr));
        } else if a == "-s" {
            opts.size = Some(geometry(&args.next()?)?);
        } else if let Some(g) = a.strip_prefix("-s") {
            opts.size = Some(geometry(g)?);
        } else if a == "-x" {
            opts.first = true;
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
fn boot(root: PathBuf, modules: Level, screen: Arc<dyn Screen>) -> Result<Prog, Error> {
    let platform: Arc<dyn Platform> = Arc::new(platform::Emu::with_screen(screen));
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
    // The draw device and the pointer, after #c as Inferno's emu has them.
    pgrp.bind("#i", "/dev", MAFTER)?;
    pgrp.bind("#m", "/dev", MAFTER)?;
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
    // The module loader's SHA-256 with the CPU's instructions.
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        // SAFETY: std says this CPU has them; hosts do not mix CPUs that
        // differ in them.
        unsafe { purg_auth::sha256::use_hardware() };
    }
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
    // With a window the main thread runs its event loop, so the loop is
    // made here, before anything else starts.
    if opts.graphics && opts.web.is_some() {
        return usage();
    }
    let (w, h) = opts.size.unwrap_or(if opts.graphics || opts.web.is_some() {
        WINDOW
    } else {
        platform::HEADLESS
    });
    let window = if opts.graphics {
        match window::event_loop() {
            Ok(el) => {
                let screen = Arc::new(window::WindowScreen::new(
                    w,
                    h,
                    opts.scale,
                    el.create_proxy(),
                ));
                Some((el, screen))
            }
            Err(e) => {
                eprintln!("emu: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let web = match &opts.web {
        Some(addr) => match web::WebScreen::listen(addr, w, h) {
            Ok((screen, url)) => {
                eprintln!("emu: screen at {url}");
                Some(screen)
            }
            Err(e) => {
                eprintln!("emu: -w {addr}: {e}");
                return 1;
            }
        },
        None => None,
    };
    let screen: Arc<dyn Screen> = match (&window, &web) {
        (Some((_, s)), _) => s.clone(),
        (None, Some(s)) => s.clone(),
        (None, None) => Arc::new(Headless::new(w, h)),
    };
    let setup =
        engine(&opts.vm).and_then(|e| Ok((e, boot(opts.root.clone(), opts.modules, screen)?)));
    let (engine, prog) = match setup {
        Ok(v) => v,
        Err(e) => {
            eprintln!("emu: {e}");
            return 1;
        }
    };
    let procs = prog.kernel.procs.clone();
    let first = opts.first;
    let ctx = Ctx::new(prog, argv, engine);
    let exit = window.as_ref().map(|(el, _)| el.create_proxy());
    let runner = std::thread::Builder::new()
        .name(String::from(purg_vm::module_name(&path)))
        .stack_size(platform::KPROC_STACK)
        .spawn(move || {
            let mut status = status(Ok(purg_vm::run_command(ctx, &path).1));
            if !first {
                // Inferno's emu: on until the last prog has gone, then 0.
                procs.wait_empty();
                status = 0;
            }
            if let Some(p) = exit {
                let _ = p.send_event(window::Event::Exit(status));
            }
            status
        });
    let runner = match runner {
        Ok(t) => t,
        Err(e) => {
            eprintln!("emu: no free procs: {e}");
            return 1;
        }
    };
    if let Some((el, screen)) = window {
        return window::run(el, screen);
    }
    let status = runner
        .join()
        .unwrap_or_else(|_| status(Err(Error::new("prog thread panicked"))));
    if let Some(web) = web {
        web.shutdown();
    }
    status
}

/// The emu's exit status with `-x` for the first prog's result: 0 when
/// its exit status is empty, 1 otherwise or when it failed to run.
fn status(res: Result<Result<String, Error>, Error>) -> u8 {
    match res.and_then(|r| r) {
        Ok(status) if status.is_empty() => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("emu: {e}");
            1
        }
    }
}
