//! m3: file2chan, a pipe export/mount, and kill of a blocked read.
//!
//! `m3 --serve-file path` serves `path` and writes a ready byte to fd 1.
//! `m3 --export` serves `/` on fd 0. `m3 --block` writes a ready byte to fd 1
//! and then reads fd 0 until it is killed.

use std::io::Read;
use std::os::fd::AsRawFd;

use purgatorio::abi::mode;
use purgatorio::abi::mount::{MCREATE, MREPL};
use purgatorio::abi::pctl;
use purgatorio::file2chan::{self, FileIO};
use purgatorio::{Context, Error, exits};

const SELF: &str = "/bin/test/m3.wasm";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--serve-file") => serve_file(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--export") => purgatorio::export(0, "/", 0),
        Some("--block") => block(),
        _ => checks(),
    }
}

fn block() -> ! {
    let _ = file2chan::write_fd(1, b"R");
    let mut buf = [0u8; 16];
    match std::io::stdin().read(&mut buf) {
        Ok(0) => exits("eof"),
        Ok(n) => exits(&format!("read {n}")),
        Err(_) => exits(&purgatorio::errstr()),
    }
}

fn checks() -> Result<(), Error> {
    let mut failed = 0;
    for (name, f) in [
        ("file2chan", file2chan as fn() -> Result<(), String>),
        ("export", export_loop),
        ("kill", kill_blocked),
    ] {
        match f() {
            Ok(()) => println!("ok {name}"),
            Err(e) => {
                println!("FAIL {name}: {e}");
                failed += 1;
            }
        }
    }
    if failed != 0 {
        exits("fail:m3");
    }
    Ok(())
}

fn file2chan() -> Result<(), String> {
    let path = "/tmp/m3-file2chan";
    let _ = std::fs::remove_file(path);
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    purgatorio::spawn(
        SELF,
        &[
            String::from("m3"),
            String::from("--serve-file"),
            String::from(path),
        ],
        pctl::NEWFD,
        &[(w.as_raw_fd(), 1), (2, 2)],
    )
    .map_err(|e| e.0)?;
    drop(w);
    let mut b = [0u8; 1];
    std::fs::File::from(r)
        .read_exact(&mut b)
        .map_err(|e| e.to_string())?;
    std::fs::write(path, b"xyz").map_err(|e| e.to_string())?;
    let got = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if got != "xyz" {
        return Err(format!("got {got:?}"));
    }
    Ok(())
}

fn serve_file(path: &str) -> Result<(), Error> {
    let (dir, name) = path
        .rsplit_once('/')
        .ok_or_else(|| Error::from("bad path"))?;
    let io = file2chan::file2chan(dir, name)?;
    file2chan::write_fd(1, b"R")?;
    loop {
        let Some(req) = io.recv()? else { return Ok(()) };
        if let Err(e) = answer(&io, req) {
            eprintln!("serve: {e}");
            return Err(e);
        }
    }
}

fn answer(_io: &FileIO, req: file2chan::Request) -> Result<(), Error> {
    static DATA: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
    let mut data = DATA.lock().unwrap_or_else(|e| e.into_inner());
    if req.is_trunc() {
        data.clear();
        return req.respond_write(0);
    }
    if req.is_read() {
        let off = req.offset as usize;
        let n = req.count() as usize;
        let slice = if off >= data.len() {
            Vec::new()
        } else {
            data[off..(off + n).min(data.len())].to_vec()
        };
        drop(data);
        return req.respond_read(&slice);
    }
    let bytes = req.data().to_vec();
    let off = req.offset as usize;
    if off > data.len() {
        data.resize(off, 0);
    }
    let end = (off + bytes.len()).max(data.len());
    data.resize(end, 0);
    data[off..off + bytes.len()].copy_from_slice(&bytes);
    drop(data);
    req.respond_write(bytes.len() as u32)
}

fn export_loop() -> Result<(), String> {
    let dir = "/tmp/m3-mount";
    let _ = std::fs::create_dir_all(dir);
    let [a, b] = purgatorio::pipe().map_err(|e| e.0)?;
    purgatorio::spawn(
        SELF,
        &[String::from("m3"), String::from("--export")],
        pctl::NEWFD,
        &[(a.as_raw_fd(), 0)],
    )
    .map_err(|e| e.0)?;
    drop(a);
    purgatorio::mount(b.as_raw_fd(), -1, dir, MREPL | MCREATE, "").map_err(|e| e.0)?;
    drop(b);
    let nested = format!("{dir}/note");
    std::fs::write(&nested, b"via-mount").map_err(|e| e.to_string())?;
    let got = std::fs::read_to_string("/note").map_err(|e| e.to_string())?;
    if got != "via-mount" {
        return Err(format!("exported file is {got:?}"));
    }
    purgatorio::unmount(None, dir).ok();
    Ok(())
}

fn kill_blocked() -> Result<(), String> {
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    let [ready_r, ready_w] = purgatorio::pipe().map_err(|e| e.0)?;
    let pid = purgatorio::spawn(
        SELF,
        &[String::from("m3"), String::from("--block")],
        pctl::NEWFD,
        &[(r.as_raw_fd(), 0), (ready_w.as_raw_fd(), 1)],
    )
    .map_err(|e| e.0)?;
    drop(r);
    drop(ready_w);
    let mut b = [0u8; 1];
    std::fs::File::from(ready_r)
        .read_exact(&mut b)
        .map_err(|e| e.to_string())?;
    std::fs::write(format!("/prog/{pid}/ctl"), "kill\n").map_err(|e| e.to_string())?;
    let msg = loop {
        let msg = purgatorio::wait().map_err(|e| e.0)?;
        if msg.pid == pid {
            break msg;
        }
    };
    drop(w);
    if !msg.status.contains("interrupted") {
        return Err(format!("wait {msg:?}"));
    }
    let _ = mode::OREAD;
    Ok(())
}
