//! abi: exercises the guest ABI from inside a prog, including error
//! paths and out-of-bounds pointers. Prints `ok <name>` per passing check
//! and `FAIL <name>: <why>` otherwise; exits with status `fail:abi` if any
//! check failed.

use std::io::Read;

use purgatorio::{Context, Error, abi};

type Check = fn() -> Result<(), String>;

/// Far past the end of any guest memory this test will have.
const BAD_PTR: usize = 0xffff_fff0;

#[cfg(target_os = "wasi")]
mod raw {
    #[repr(C)]
    pub struct Iovec {
        pub buf: usize,
        pub len: usize,
    }

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        pub fn fd_read(fd: i32, iovs: *const Iovec, n: usize, nread: *mut usize) -> u16;
    }
}

/// `fd_read` into an out-of-bounds buffer must fail with EFAULT, not crash
/// the host, and leave "bad address" as the errstr.
#[cfg(target_os = "wasi")]
fn bad_pointer_read() -> Result<(), String> {
    let iov = raw::Iovec {
        buf: BAD_PTR,
        len: 64,
    };
    let mut nread = 0usize;
    let errno = unsafe { raw::fd_read(0, &iov, 1, &mut nread) };
    if i32::from(errno) != abi::wasi::errno::FAULT {
        return Err(format!("errno {errno}, want EFAULT"));
    }
    expect_errstr(abi::error::EBADADDR)
}

#[cfg(not(target_os = "wasi"))]
fn bad_pointer_read() -> Result<(), String> {
    Err("only runs on wasm32-wasip1".into())
}

fn bad_pointer_stat() -> Result<(), String> {
    let mut buf = [0u8; 128];
    let n = unsafe { purgatorio::sys::stat(BAD_PTR as *const u8, 8, buf.as_mut_ptr(), buf.len()) };
    if n != -1 {
        return Err(format!("returned {n}, want -1"));
    }
    expect_errstr(abi::error::EBADADDR)
}

fn expect_errstr(want: &str) -> Result<(), String> {
    let got = purgatorio::errstr();
    if got == want {
        Ok(())
    } else {
        Err(format!("errstr {got:?}, want {want:?}"))
    }
}

fn std_open_missing() -> Result<(), String> {
    match std::fs::File::open("/nonexistent") {
        Ok(_) => Err("opened /nonexistent".into()),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("kind {:?}", e.kind())),
        Err(e) => {
            expect_errstr(abi::error::ENONEXIST)?;
            let sdk: Error = e.into();
            if sdk.0 == abi::error::ENONEXIST {
                Ok(())
            } else {
                Err(format!("Error from io::Error is {:?}", sdk.0))
            }
        }
    }
}

fn std_read_file() -> Result<(), String> {
    let mut s = String::new();
    std::fs::File::open("/dev/sysname")
        .and_then(|mut f| f.read_to_string(&mut s))
        .map_err(|e| e.to_string())?;
    if s.is_empty() {
        return Err("empty /dev/sysname".into());
    }
    Ok(())
}

fn std_read_dir() -> Result<(), String> {
    let names: Vec<String> = std::fs::read_dir("/dev")
        .map_err(|e| e.to_string())?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if names.iter().any(|n| n == "cons") {
        Ok(())
    } else {
        Err(format!("/dev lists {names:?}"))
    }
}

fn stat_dir() -> Result<(), String> {
    let d = purgatorio::stat("/dev/cons").map_err(|e| e.0)?;
    if d.name != "cons" || d.dtype != u16::from(b'c') || d.qid.is_dir() {
        return Err(format!("{d:?}"));
    }
    let root = purgatorio::stat("/").map_err(|e| e.0)?;
    if !root.qid.is_dir() {
        return Err(format!("/ is not a directory: {root:?}"));
    }
    Ok(())
}

fn fstat_stdout() -> Result<(), String> {
    let d = purgatorio::fstat(1).map_err(|e| e.0)?;
    if d.name == "cons" {
        Ok(())
    } else {
        Err(format!("fd 1 is {:?}", d.name))
    }
}

fn dirread_root() -> Result<(), String> {
    // PREOPEN_ROOT_FD is the "/" preopen, already open for reading.
    let mut names = Vec::new();
    loop {
        let batch = purgatorio::dirread(abi::PREOPEN_ROOT_FD).map_err(|e| e.0)?;
        if batch.is_empty() {
            break;
        }
        names.extend(batch.into_iter().map(|d| d.name));
    }
    for want in ["dev", "bin"] {
        if !names.iter().any(|n| n == want) {
            return Err(format!("/ lists {names:?}, missing {want}"));
        }
    }
    Ok(())
}

fn werrstr_roundtrip() -> Result<(), String> {
    purgatorio::werrstr("custom error");
    expect_errstr("custom error")
}

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    let checks: &[(&str, Check)] = &[
        ("bad_pointer_read", bad_pointer_read),
        ("bad_pointer_stat", bad_pointer_stat),
        ("std_open_missing", std_open_missing),
        ("std_read_file", std_read_file),
        ("std_read_dir", std_read_dir),
        ("stat_dir", stat_dir),
        ("fstat_stdout", fstat_stdout),
        ("dirread_root", dirread_root),
        ("werrstr_roundtrip", werrstr_roundtrip),
    ];
    let mut failed = false;
    for (name, check) in checks {
        match check() {
            Ok(()) => println!("ok {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed = true;
            }
        }
    }
    if failed {
        purgatorio::exits("fail:abi");
    }
    Ok(())
}
