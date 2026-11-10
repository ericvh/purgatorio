//! m3: file2chan, a pipe export/mount, kill of a blocked read and of a
//! computing prog, fauth with an explicit auth fd, and stream.
//!
//! `m3 --serve-file path` serves `path` and writes a ready byte to fd 1.
//! `m3 --export` serves `/` on fd 0, and `m3 --export-noauth` does so
//! without authentication. `m3 --block` writes a ready byte to fd 1 and then
//! reads fd 0 until it is killed. `m3 --stream path` writes a ready byte to
//! fd 1 and streams fd 0 into `path` until it is killed. `m3 --spin` writes
//! a ready byte to fd 1 and then computes, with no host calls, until it is
//! killed.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use purgatorio::abi::mode;
use purgatorio::abi::mount::{MCREATE, MNOAUTH, MREPL};
use purgatorio::abi::pctl;
use purgatorio::file2chan::{self, FileIO};
use purgatorio::{Context, Error, exits};

const SELF: &str = "/bin/test/m3.wasm";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--serve-file") => serve_file(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--export") => purgatorio::export(0, "/", 0),
        Some("--export-noauth") => purgatorio::export(0, "/", MNOAUTH),
        Some("--stream") => stream_child(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--block") => block(),
        Some("--spin") => spin(),
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

fn spin() -> ! {
    let _ = file2chan::write_fd(1, b"R");
    let mut x = 0u64;
    loop {
        x = std::hint::black_box(x.wrapping_add(1));
    }
}

fn checks() -> Result<(), Error> {
    let mut failed = 0;
    for (name, f) in [
        ("file2chan", file2chan as fn() -> Result<(), String>),
        ("export", export_loop),
        ("kill", kill_blocked),
        ("kill_spin", kill_spin),
        ("fauth", fauth_mount),
        ("fauth_bad_proof", fauth_bad_proof),
        ("fauth_not_required", fauth_not_required),
        ("noauth_rejected", noauth_rejected),
        ("amount", amount),
        ("stream_pipe_file", stream_pipe_file),
        ("stream_file_pipe", stream_file_pipe),
        ("stream_short_write", stream_short_write),
        ("stream_kill", stream_kill),
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
    let nested = format!("{dir}/tmp/m3-note");
    std::fs::write(&nested, b"via-mount").map_err(|e| e.to_string())?;
    let got = std::fs::read_to_string("/tmp/m3-note").map_err(|e| e.to_string())?;
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

/// A prog computing with no host calls has no call to interrupt, so the
/// VM's periodic note check (wasmi fuel, wasmtime epochs) stops it.
fn kill_spin() -> Result<(), String> {
    let [ready_r, ready_w] = purgatorio::pipe().map_err(|e| e.0)?;
    let pid = purgatorio::spawn(
        SELF,
        &[String::from("m3"), String::from("--spin")],
        pctl::NEWFD,
        &[(ready_w.as_raw_fd(), 1)],
    )
    .map_err(|e| e.0)?;
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
    if msg.status != "killed" {
        return Err(format!("wait {msg:?}"));
    }
    Ok(())
}

/// Spawns `m3 <flag>` serving `/` on one end of a pipe and returns the
/// other end.
fn exporter(flag: &str) -> Result<OwnedFd, String> {
    let [a, b] = purgatorio::pipe().map_err(|e| e.0)?;
    purgatorio::spawn(
        SELF,
        &[String::from("m3"), String::from(flag)],
        pctl::NEWFD,
        &[(a.as_raw_fd(), 0)],
    )
    .map_err(|e| e.0)?;
    drop(a);
    Ok(b)
}

/// Reads the fixture public key through the mount at `dir`, then unmounts.
fn read_through(dir: &str) -> Result<(), String> {
    let via = std::fs::read(format!("{dir}/lib/test/auth.key.pub")).map_err(|e| e.to_string());
    purgatorio::unmount(None, dir).ok();
    let via = via?;
    let direct = std::fs::read("/lib/test/auth.key.pub").map_err(|e| e.to_string())?;
    if via != direct {
        return Err(format!("read {} bytes through the mount", via.len()));
    }
    Ok(())
}

fn mtpt(name: &str) -> Result<String, String> {
    let dir = format!("/tmp/m3-{name}");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn want_err<T>(r: Result<T, Error>, want: &str) -> Result<(), String> {
    match r {
        Ok(_) => Err(format!("succeeded, want {want:?}")),
        Err(e) if e.0 == want => Ok(()),
        Err(e) => Err(format!("error {:?}, want {want:?}", e.0)),
    }
}

/// fauth, the Ed25519 exchange done by the guest, then mount with the afd.
fn fauth_mount() -> Result<(), String> {
    let dir = mtpt("fauth")?;
    let conn = exporter("--export")?;
    let afd = purgatorio::fauth(conn.as_raw_fd(), "").map_err(|e| e.0)?;
    let mut f = std::fs::File::from(afd);
    let mut challenge = [0u8; purgatorio::abi::auth::CHALLENGE];
    f.read_exact(&mut challenge).map_err(|e| e.to_string())?;
    let proof = purgatorio::auth::proof(&challenge, &purgatorio::auth::user()).map_err(|e| e.0)?;
    f.write_all(&proof).map_err(|e| e.to_string())?;
    purgatorio::mount(conn.as_raw_fd(), f.as_raw_fd(), &dir, MREPL, "").map_err(|e| e.0)?;
    drop(f);
    drop(conn);
    read_through(&dir)
}

/// A wrong signature fails the write, and the afd does not let the attach in.
fn fauth_bad_proof() -> Result<(), String> {
    let dir = mtpt("fauth-bad")?;
    let conn = exporter("--export")?;
    let afd = purgatorio::fauth(conn.as_raw_fd(), "").map_err(|e| e.0)?;
    let mut f = std::fs::File::from(afd);
    let mut challenge = [0u8; purgatorio::abi::auth::CHALLENGE];
    f.read_exact(&mut challenge).map_err(|e| e.to_string())?;
    let r = f.write_all(&[0u8; purgatorio::abi::auth::PROOF]);
    want_err(r.map_err(Error::from), purgatorio::abi::auth::FAILED)?;
    let r = purgatorio::mount(conn.as_raw_fd(), f.as_raw_fd(), &dir, MREPL, "");
    want_err(r, purgatorio::abi::auth::REQUIRED)
}

/// A server without a key answers fauth with "authentication not
/// required", and a plain mount on the same fd still works.
fn fauth_not_required() -> Result<(), String> {
    let dir = mtpt("fauth-none")?;
    let conn = exporter("--export-noauth")?;
    let r = purgatorio::fauth(conn.as_raw_fd(), "");
    want_err(r, purgatorio::abi::auth::NOT_REQUIRED)?;
    purgatorio::mount(conn.as_raw_fd(), -1, &dir, MREPL, "").map_err(|e| e.0)?;
    drop(conn);
    read_through(&dir)
}

/// `-A` (MNOAUTH) against a server with a key: the attach is refused.
fn noauth_rejected() -> Result<(), String> {
    let dir = mtpt("noauth")?;
    let conn = exporter("--export")?;
    let r = purgatorio::mount(conn.as_raw_fd(), -1, &dir, MREPL | MNOAUTH, "");
    want_err(r, purgatorio::abi::auth::REQUIRED)
}

/// The SDK's amount, against servers with and without a key.
fn amount() -> Result<(), String> {
    for flag in ["--export", "--export-noauth"] {
        let dir = mtpt("amount")?;
        let conn = exporter(flag)?;
        purgatorio::auth::amount(conn.as_raw_fd(), &dir, MREPL, "")
            .map_err(|e| format!("{flag}: {}", e.0))?;
        drop(conn);
        read_through(&dir).map_err(|e| format!("{flag}: {e}"))?;
    }
    Ok(())
}

const STREAM_DATA: &[u8] = b"the quick brown fox jumps over the lazy dog";

fn stream_pipe_file() -> Result<(), String> {
    let path = "/tmp/m3-stream-file";
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    std::fs::File::from(w)
        .write_all(STREAM_DATA)
        .map_err(|e| e.to_string())?;
    let out = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let n = purgatorio::stream(r.as_raw_fd(), out.as_raw_fd(), 4).map_err(|e| e.0)?;
    drop(out);
    let got = std::fs::read(path).map_err(|e| e.to_string())?;
    if n != STREAM_DATA.len() || got != STREAM_DATA {
        return Err(format!("stream returned {n}, file has {got:?}"));
    }
    Ok(())
}

fn stream_file_pipe() -> Result<(), String> {
    let path = "/tmp/m3-stream-src";
    std::fs::write(path, STREAM_DATA).map_err(|e| e.to_string())?;
    let src = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    let n = purgatorio::stream(src.as_raw_fd(), w.as_raw_fd(), 5).map_err(|e| e.0)?;
    drop(w);
    let mut got = Vec::new();
    std::fs::File::from(r)
        .read_to_end(&mut got)
        .map_err(|e| e.to_string())?;
    if n != STREAM_DATA.len() || got != STREAM_DATA {
        return Err(format!("stream returned {n}, pipe gave {got:?}"));
    }
    Ok(())
}

/// A failed write ends the copy: that buffer is not counted, and the
/// errstr says why.
fn stream_short_write() -> Result<(), String> {
    let path = "/tmp/m3-stream-src";
    std::fs::write(path, STREAM_DATA).map_err(|e| e.to_string())?;
    let src = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let dst = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let n = purgatorio::stream(src.as_raw_fd(), dst.as_raw_fd(), 4).map_err(|e| e.0)?;
    if n != 0 {
        return Err(format!("stream returned {n}"));
    }
    let e = purgatorio::errstr();
    if e != purgatorio::abi::error::EBADUSEFD {
        return Err(format!("errstr {e:?}"));
    }
    Ok(())
}

fn stream_child(path: &str) -> ! {
    let out = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => exits(&e.to_string()),
    };
    let _ = file2chan::write_fd(1, b"R");
    match purgatorio::stream(0, out.as_raw_fd(), 3) {
        Ok(n) => exits(&format!("stream {n} {}", purgatorio::errstr())),
        Err(e) => exits(&e.0),
    }
}

/// A prog blocked in stream is interrupted by kill like a blocked read, and
/// returns the count it copied.
fn stream_kill() -> Result<(), String> {
    let path = "/tmp/m3-stream-kill";
    let _ = std::fs::remove_file(path);
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    let [ready_r, ready_w] = purgatorio::pipe().map_err(|e| e.0)?;
    let pid = purgatorio::spawn(
        SELF,
        &[
            String::from("m3"),
            String::from("--stream"),
            String::from(path),
        ],
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
    let mut w = std::fs::File::from(w);
    w.write_all(b"abc").map_err(|e| e.to_string())?;
    // A full buffer goes straight to the file. Wait for it, so that the
    // child is blocked in its next read when the kill comes.
    let mut copied = false;
    for _ in 0..500 {
        if std::fs::read(path).is_ok_and(|d| d == b"abc") {
            copied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !copied {
        return Err(String::from("the first buffer never reached the file"));
    }
    std::fs::write(format!("/prog/{pid}/ctl"), "kill\n").map_err(|e| e.to_string())?;
    let msg = loop {
        let msg = purgatorio::wait().map_err(|e| e.0)?;
        if msg.pid == pid {
            break msg;
        }
    };
    drop(w);
    if msg.status != "stream 3 interrupted" {
        return Err(format!("wait {msg:?}"));
    }
    Ok(())
}
