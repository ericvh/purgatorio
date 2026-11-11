//! Stock `wasm32-wasip1` program. It uses Rust `std` only, so every file
//! operation goes through WASI preview 1 rather than the `purgatorio` module.
//! The ABI section lets our build pipeline accept the module; a third-party
//! binary without it still loads.

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

// Only wasm has this section; Mach-O rejects a section name with no segment.
#[used]
#[cfg_attr(target_family = "wasm", unsafe(link_section = "purgatorio.abi"))]
static ABI: [u8; 3] = *b"0.9";

#[link(wasm_import_module = "wasi_snapshot_preview1")]
unsafe extern "C" {
    fn clock_res_get(id: i32, resolution: *mut u64) -> i32;
    fn fd_advise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
    fn fd_allocate(fd: i32, offset: i64, len: i64) -> i32;
    fn proc_raise(sig: i32) -> i32;
    fn sock_accept(fd: i32, flags: i32, result: *mut i32) -> i32;
}

fn ok(name: &str) {
    println!("ok {name}");
}

fn fail(name: &str, msg: impl core::fmt::Display) -> ! {
    eprintln!("FAIL {name}: {msg}");
    std::process::exit(1);
}

fn main() {
    match fs::read_to_string("/lib/probe/hello.txt") {
        Ok(text) if text.contains("hello") => ok("read"),
        Ok(text) => fail("read", format!("unexpected {text:?}")),
        Err(e) => fail("read", e),
    }

    let names = match fs::read_dir("/lib/probe") {
        Ok(dir) => dir
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(e) => fail("readdir", e),
    };
    if names.iter().any(|n| n == "hello.txt") {
        ok("readdir");
    } else {
        fail("readdir", format!("{names:?}"));
    }

    // A name that starts with `#` is a device, for open, stat and readdir.
    let dev = fs::read("#c/sysname").unwrap_or_else(|e| fail("devname", e));
    let sysname = fs::read("/dev/sysname").unwrap_or_else(|e| fail("devname", e));
    if dev != sysname {
        fail("devname", format!("{dev:?} != {sysname:?}"));
    }
    match fs::metadata("#c/sysname") {
        Ok(m) if !m.is_dir() => {}
        Ok(_) => fail("devname", "#c/sysname is a directory"),
        Err(e) => fail("devname", e),
    }
    let mut dir = fs::read_dir("#c").unwrap_or_else(|e| fail("devname", e));
    if !dir.any(|e| e.is_ok_and(|e| e.file_name() == "sysname")) {
        fail("devname", "no sysname in #c");
    }
    ok("devname");

    if let Err(e) = fs::create_dir_all("/tmp/wasi-d") {
        fail("mkdir", e);
    }
    if let Err(e) = fs::write("/tmp/wasi-d/a", "xyz") {
        fail("write", e);
    }
    let mut extra = match fs::OpenOptions::new().append(true).open("/tmp/wasi-d/a") {
        Ok(f) => f,
        Err(e) => fail("append", e),
    };
    if let Err(e) = write!(extra, "Q") {
        fail("append", e);
    }
    drop(extra);
    match fs::read("/tmp/wasi-d/a") {
        Ok(bytes) if bytes == b"xyzQ" => {}
        Ok(bytes) => fail("append", format!("{bytes:?}")),
        Err(e) => fail("append", e),
    }
    if let Err(e) = fs::rename("/tmp/wasi-d/a", "/tmp/wasi-d/b") {
        fail("rename", e);
    }
    match fs::metadata("/tmp/wasi-d/b") {
        Ok(m) if m.len() == 4 && m.is_file() => ok("files"),
        Ok(m) => fail("files", format!("len {} dir {}", m.len(), m.is_dir())),
        Err(e) => fail("files", e),
    }

    let file = match fs::File::open("/tmp/wasi-d/b") {
        Ok(f) => f,
        Err(e) => fail("advise", e),
    };
    let fd = file.as_raw_fd();
    let rc = unsafe { fd_advise(fd, 0, 4, 0) };
    if rc != 0 {
        fail("advise", rc);
    }
    let rc = unsafe { fd_advise(fd, 0, 0, 9) };
    if rc != 28 {
        fail("advise", format!("bad advice errno {rc}"));
    }
    let rc = unsafe { fd_allocate(fd, 0, 8) };
    drop(file);
    if rc != 0 {
        fail("allocate", rc);
    }
    match fs::metadata("/tmp/wasi-d/b") {
        Ok(m) if m.len() == 8 => ok("allocate"),
        Ok(m) => fail("allocate", m.len()),
        Err(e) => fail("allocate", e),
    }

    match fs::hard_link("/tmp/wasi-d/b", "/tmp/wasi-d/c") {
        Err(_) => ok("nolink"),
        Ok(()) => fail("nolink", "hard link was created"),
    }
    match fs::read_link("/tmp/wasi-d/b") {
        Err(_) => ok("noreadlink"),
        Ok(p) => fail("noreadlink", p.display()),
    }

    let mut resolution = 0u64;
    let rc = unsafe { clock_res_get(0, &mut resolution) };
    if rc != 0 || resolution == 0 {
        fail("clock", format!("errno {rc} res {resolution}"));
    }
    let rc = unsafe { clock_res_get(99, &mut resolution) };
    if rc != 58 {
        fail("clock", format!("unknown clock errno {rc}"));
    }
    ok("clock");

    let rc = unsafe { proc_raise(0) };
    if rc != 28 {
        fail("raise", rc);
    }
    ok("raise");

    let plain = match fs::File::open("/lib/probe/hello.txt") {
        Ok(f) => f,
        Err(e) => fail("sock", e),
    };
    let mut accepted = -1i32;
    let rc = unsafe { sock_accept(plain.as_raw_fd(), 0, &mut accepted) };
    if rc != 57 {
        fail("sock", format!("errno {rc}"));
    }
    ok("sock");

    if let Err(e) = fs::remove_file("/tmp/wasi-d/b") {
        fail("cleanup", e);
    }
    if let Err(e) = fs::remove_dir("/tmp/wasi-d") {
        fail("cleanup", e);
    }

    thread::sleep(Duration::from_millis(1));
    ok("sleep");

    if std::env::args().next().is_none() {
        fail("args", "no argv");
    }
    ok("args");
}
