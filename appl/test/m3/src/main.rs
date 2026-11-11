//! m3: file2chan, a pipe export/mount, kill of a blocked read and of a
//! computing prog, kill of `sh` wherever it is blocked, fauth with an
//! explicit auth fd, and stream.
//!
//! `m3 --serve-file path` serves `path` and writes a ready byte to fd 1.
//! `m3 --export` serves `/` on fd 0, `m3 --export-noauth` does so
//! without authentication, and `m3 --export-crypt` only to encrypted
//! conversations. `m3 --block` writes a ready byte to fd 1 and then
//! reads fd 0 until it is killed. `m3 --stream path` writes a ready byte to
//! fd 1 and streams fd 0 into `path` until it is killed. `m3 --spin` writes
//! a ready byte to fd 1 and then computes, with no host calls, until it is
//! killed. `m3 --nap` writes a ready byte to fd 1 and sleeps until it is
//! killed. `m3 --pump frame log` copies fd 0 to fd 1 and appends what it
//! copies to `log`; when `frame` is a number it flips a bit in the last
//! byte of that frame (a 9P message or a record) on the way.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use purgatorio::abi::mode;
use purgatorio::abi::mount::{MCREATE, MNOAUTH, MREPL, MSRVAUTH};
use purgatorio::abi::pctl;
use purgatorio::file2chan::{self, FileIO};
use purgatorio::{Context, Error, exits};

const SELF: &str = "/bin/test/m3.wasm";
const SH: &str = "/bin/sh.wasm";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--serve-file") => serve_file(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--export") => purgatorio::export(0, "/", 0),
        Some("--export-noauth") => purgatorio::export(0, "/", MNOAUTH),
        Some("--export-crypt") => purgatorio::export(0, "/", purgatorio::abi::export::ENCRYPT),
        Some("--stream") => stream_child(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--block") => block(),
        Some("--pump") => pump(
            args.get(2).and_then(|f| f.parse().ok()),
            args.get(3).map(String::as_str).unwrap_or(""),
        ),
        Some("--spin") => spin(),
        Some("--nap") => nap(),
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

fn nap() -> ! {
    let _ = file2chan::write_fd(1, b"R");
    std::thread::sleep(std::time::Duration::from_secs(3600));
    exits("woke");
}

fn checks() -> Result<(), Error> {
    let mut failed = 0;
    for (name, f) in [
        ("file2chan", file2chan as fn() -> Result<(), String>),
        ("export", export_loop),
        ("kill", kill_blocked),
        ("kill_spin", kill_spin),
        ("kill_nap", kill_nap),
        ("kill_sh_wait", kill_sh_wait),
        ("kill_sh_read", kill_sh_read),
        ("kill_sh_spawn", kill_sh_spawn),
        ("fauth", fauth_mount),
        ("fauth_bad_proof", fauth_bad_proof),
        ("fauth_not_required", fauth_not_required),
        ("noauth_rejected", noauth_rejected),
        ("amount", amount),
        ("crypt_wire", crypt_wire),
        ("crypt_clear", crypt_clear),
        ("crypt_tamper", crypt_tamper),
        ("crypt_server", crypt_server),
        ("crypt_required", crypt_required),
        ("crypt_reuse", crypt_reuse),
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
    // The kill ends the prog: the read does not return `interrupted` to it.
    if msg.status != "killed" {
        return Err(format!("wait {msg:?}"));
    }
    let _ = mode::OREAD;
    Ok(())
}

/// The exit message of child `pid`.
fn wait_for(pid: u32) -> Result<purgatorio::Waitmsg, String> {
    loop {
        let msg = purgatorio::wait().map_err(|e| e.0)?;
        if msg.pid == pid {
            return Ok(msg);
        }
    }
}

fn kill(pid: u32) -> Result<(), String> {
    std::fs::write(format!("/prog/{pid}/ctl"), "kill\n").map_err(|e| e.to_string())
}

/// The progs whose parent is `ppid`.
fn children(ppid: u32) -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/prog") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/prog/{pid}/status"))
                .is_ok_and(|s| s.split_whitespace().nth(1) == Some(&*ppid.to_string()))
        })
        .collect()
}

/// Kills `pid` once it has been left `ms` milliseconds to block, and
/// checks that it ended as killed.
fn kill_after(pid: u32, ms: u64) -> Result<(), String> {
    std::thread::sleep(std::time::Duration::from_millis(ms));
    kill(pid)?;
    let msg = wait_for(pid)?;
    if msg.status != "killed" {
        return Err(format!("wait {msg:?}"));
    }
    Ok(())
}

/// A prog killed in a sleep ends as killed.
fn kill_nap() -> Result<(), String> {
    let [ready_r, ready_w] = purgatorio::pipe().map_err(|e| e.0)?;
    let pid = purgatorio::spawn(
        SELF,
        &[String::from("m3"), String::from("--nap")],
        pctl::NEWFD,
        &[(ready_w.as_raw_fd(), 1)],
    )
    .map_err(|e| e.0)?;
    drop(ready_w);
    let mut b = [0u8; 1];
    std::fs::File::from(ready_r)
        .read_exact(&mut b)
        .map_err(|e| e.to_string())?;
    kill_after(pid, 50)
}

/// Starts `sh` with `argv`, its fd 0 `stdin` or `/dev/null`, fd 1
/// `/dev/null` and fd 2 this prog's.
fn sh(argv: &[&str], stdin: Option<&OwnedFd>) -> Result<u32, String> {
    let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|e| e.to_string())?;
    let fd0 = stdin.map_or(null.as_raw_fd(), |f| f.as_raw_fd());
    let fdmap = [(fd0, 0), (null.as_raw_fd(), 1), (2, 2)];
    purgatorio::spawn(SH, &argv, pctl::NEWFD, &fdmap).map_err(|e| e.0)
}

/// `sh` killed while it waits for a command ends as killed, not with the
/// error `wait` would return (issue #102).
fn kill_sh_wait() -> Result<(), String> {
    let pid = sh(&["sh", "-c", "sleep 300 & wait"], None)?;
    let mut job = Vec::new();
    for _ in 0..1000 {
        job = children(pid);
        if !job.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let res = kill_after(pid, 100);
    for p in job {
        let _ = kill(p);
    }
    res
}

/// `sh` killed while it reads commands from a pipe ends as killed.
fn kill_sh_read() -> Result<(), String> {
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    let pid = sh(&["sh"], Some(&r))?;
    drop(r);
    let res = kill_after(pid, 100);
    drop(w);
    res
}

/// `sh` killed while it starts a command, here one whose module is served
/// by this prog and never comes, ends as killed rather than reporting the
/// command's error (issue #102).
fn kill_sh_spawn() -> Result<(), String> {
    let path = "/tmp/m3-slow.wasm";
    let io = file2chan::file2chan("/tmp", "m3-slow.wasm").map_err(|e| e.0)?;
    let pid = sh(&["sh", "-c", path], None)?;
    // The first read of the module: sh is in `spawn_prog` until it is
    // answered.
    let req = io.recv().map_err(|e| e.0)?;
    if req.as_ref().is_none_or(|r| !r.is_read()) {
        return Err(String::from("no read of the module"));
    }
    let res = kill_after(pid, 0);
    drop(req);
    drop(io);
    let _ = purgatorio::unmount(None, path);
    res
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
    // The offer and the kernel's ephemeral key: a version 3 proof.
    let mut signed = [0u8; purgatorio::abi::auth::SIGNED];
    f.read_exact(&mut signed).map_err(|e| e.to_string())?;
    let proof = purgatorio::auth::proof(&signed, &purgatorio::auth::user()).map_err(|e| e.0)?;
    if proof.first() != Some(&purgatorio::abi::auth::PROOF_V3) {
        return Err(String::from("not a version 3 proof"));
    }
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

/// One direction of a relay: copies fd 0 to fd 1, appending what passes
/// to `log` before passing it on, until either side fails or it is
/// killed. With `flip`, the last byte of that frame is changed.
///
/// When it stops it kills the other direction, whose pid is in
/// `<log>.peer`: that one holds both pipes too, so neither end would see
/// the hangup while it lives.
fn pump(flip: Option<usize>, log: &str) -> ! {
    let stop = || -> ! {
        if let Ok(pid) = std::fs::read_to_string(format!("{log}.peer")) {
            let _ = std::fs::write(format!("/prog/{}/ctl", pid.trim()), "kill\n");
        }
        exits("")
    };
    let mut seen = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut input = std::io::stdin();
    let mut output = std::io::stdout();
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) | Err(_) => stop(),
            Ok(n) => n,
        };
        let start = seen.len();
        seen.extend_from_slice(&buf[..n]);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = f.write_all(&buf[..n]);
        }
        if let Some(end) = flip.and_then(|k| frame_end(&seen, k))
            && (start..start + n).contains(&(end - 1))
        {
            buf[end - 1 - start] ^= 1;
        }
        if output
            .write_all(&buf[..n])
            .and_then(|()| output.flush())
            .is_err()
        {
            stop();
        }
    }
}

/// Where frame `k` of `log` ends, once its size is in. 9P messages and
/// the records after an authenticated attach both start with their size.
fn frame_end(log: &[u8], k: usize) -> Option<usize> {
    let mut pos = 0;
    for i in 0..=k {
        let size = u32::from_le_bytes(log.get(pos..pos + 4)?.try_into().ok()?) as usize;
        if i == k {
            return Some(pos + size);
        }
        pos += size;
    }
    None
}

const WIRE_NAME: &str = "m3-wire-secret-name";
const WIRE_DATA: &[u8] = b"a payload nobody on the wire may read";

/// What a relay between a mount and `m3 <flag>` saw.
struct Wire {
    /// The mount's result, then what was written through it and read back.
    mounted: Result<(), String>,
    up: Vec<u8>,
    down: Vec<u8>,
}

/// Mounts `m3 <flag>` through a relay of two `m3 --pump` progs, writes a
/// file through the mount and reads it back, then stops the relay.
fn through_relay(flag: &str, flags: u32, flip: Option<(bool, usize)>) -> Result<Wire, String> {
    let dir = mtpt("wire")?;
    let logs = ["/tmp/m3-wire-up", "/tmp/m3-wire-down"];
    for l in logs {
        let _ = std::fs::remove_file(l);
        let _ = std::fs::remove_file(format!("{l}.peer"));
    }
    let server = exporter(flag)?;
    let [c, r] = purgatorio::pipe().map_err(|e| e.0)?;
    let frame = |up: bool| match flip {
        Some((u, k)) if u == up => k.to_string(),
        _ => String::from("-"),
    };
    let mut pids = Vec::new();
    for (up, from, to, log) in [(true, &r, &server, logs[0]), (false, &server, &r, logs[1])] {
        let pid = purgatorio::spawn(
            SELF,
            &[
                String::from("m3"),
                String::from("--pump"),
                frame(up),
                String::from(log),
            ],
            pctl::NEWFD,
            &[(from.as_raw_fd(), 0), (to.as_raw_fd(), 1)],
        )
        .map_err(|e| e.0)?;
        pids.push(pid);
    }
    for (log, peer) in [(logs[0], pids[1]), (logs[1], pids[0])] {
        std::fs::write(format!("{log}.peer"), peer.to_string()).map_err(|e| e.to_string())?;
    }
    drop(r);
    drop(server);
    let mounted =
        purgatorio::auth::amount(c.as_raw_fd(), &dir, MREPL | MCREATE | (flags & MNOAUTH), "")
            .map_err(|e| e.0);
    drop(c);
    let used = mounted.clone().and_then(|()| {
        let path = format!("{dir}/tmp/{WIRE_NAME}");
        let r = std::fs::write(&path, WIRE_DATA)
            .and_then(|()| std::fs::read(&path))
            .map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&path);
        purgatorio::unmount(None, &dir).ok();
        match r? {
            d if d == WIRE_DATA => Ok(()),
            d => Err(format!("read back {} bytes", d.len())),
        }
    });
    for pid in &pids {
        let _ = std::fs::write(format!("/prog/{pid}/ctl"), "kill\n");
    }
    let mut left = pids.clone();
    while !left.is_empty() {
        let msg = purgatorio::wait().map_err(|e| e.0)?;
        left.retain(|p| *p != msg.pid);
    }
    let up = std::fs::read(logs[0]).unwrap_or_default();
    let down = std::fs::read(logs[1]).unwrap_or_default();
    Ok(Wire {
        mounted: mounted.and(used),
        up,
        down,
    })
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// After an authenticated attach nothing on the wire is plaintext Styx:
/// neither a file's name nor its contents pass in the clear.
fn crypt_wire() -> Result<(), String> {
    let w = through_relay("--export", 0, None)?;
    w.mounted?;
    if !contains(&w.up, purgatorio::auth::user().as_bytes()) {
        return Err(String::from("Tauth's user name is not on the wire"));
    }
    for (dir, log) in [("up", &w.up), ("down", &w.down)] {
        if contains(log, WIRE_NAME.as_bytes()) || contains(log, WIRE_DATA) {
            return Err(format!("{dir}: plaintext on the wire"));
        }
    }
    Ok(())
}

/// `-A` against a server without a key stays in the clear.
fn crypt_clear() -> Result<(), String> {
    let w = through_relay("--export-noauth", MNOAUTH, None)?;
    w.mounted?;
    if !contains(&w.up, WIRE_NAME.as_bytes()) || !contains(&w.up, WIRE_DATA) {
        return Err(String::from("the clear conversation is not on the wire"));
    }
    Ok(())
}

/// A byte changed in the first record each way (frame 4: after Tversion,
/// Tauth, the Tread of the offer and the Twrite of the proof) fails the
/// mount, with no hang.
fn crypt_tamper() -> Result<(), String> {
    let w = through_relay("--export", 0, Some((false, 4)))?;
    want_err(w.mounted.map_err(Error::from), "bad record mac")?;
    let w = through_relay("--export", 0, Some((true, 4)))?;
    want_err(
        w.mounted.map_err(Error::from),
        purgatorio::abi::error::EHUNGUP,
    )
}

/// The server proves itself with the test key, which this namespace trusts
/// as a server's: a mount and an `amount` that require it work, and one
/// against a server that asks for no proof is refused.
fn crypt_server() -> Result<(), String> {
    let dir = mtpt("server")?;
    let conn = exporter("--export")?;
    purgatorio::mount(conn.as_raw_fd(), -1, &dir, MREPL | MSRVAUTH, "").map_err(|e| e.0)?;
    drop(conn);
    read_through(&dir)?;
    let conn = exporter("--export")?;
    purgatorio::auth::amount(conn.as_raw_fd(), &dir, MREPL | MSRVAUTH, "").map_err(|e| e.0)?;
    drop(conn);
    read_through(&dir)?;
    let conn = exporter("--export-noauth")?;
    let r = purgatorio::auth::amount(conn.as_raw_fd(), &dir, MREPL | MSRVAUTH, "");
    want_err(r, purgatorio::abi::auth::SERVER_NONE)
}

/// An export that requires encryption refuses a version 2 proof and takes
/// a version 3 one.
fn crypt_required() -> Result<(), String> {
    let dir = mtpt("required")?;
    let conn = exporter("--export-crypt")?;
    let afd = purgatorio::fauth(conn.as_raw_fd(), "").map_err(|e| e.0)?;
    let mut f = std::fs::File::from(afd);
    let mut challenge = [0u8; purgatorio::abi::auth::CHALLENGE];
    f.read_exact(&mut challenge).map_err(|e| e.to_string())?;
    let proof = purgatorio::auth::proof(&challenge, &purgatorio::auth::user()).map_err(|e| e.0)?;
    let r = f.write_all(&proof);
    want_err(
        r.map_err(Error::from),
        purgatorio::abi::auth::ENCRYPTION_REQUIRED,
    )?;
    drop(f);
    drop(conn);
    let conn = exporter("--export-crypt")?;
    purgatorio::auth::amount(conn.as_raw_fd(), &dir, MREPL, "").map_err(|e| e.0)?;
    drop(conn);
    read_through(&dir)
}

/// After a refused version 3 proof the conversation is records: a plain
/// mount on the same fd says so instead of failing obscurely.
fn crypt_reuse() -> Result<(), String> {
    let dir = mtpt("reuse")?;
    let conn = exporter("--export")?;
    let afd = purgatorio::fauth(conn.as_raw_fd(), "").map_err(|e| e.0)?;
    let mut f = std::fs::File::from(afd);
    let mut signed = [0u8; purgatorio::abi::auth::SIGNED];
    f.read_exact(&mut signed).map_err(|e| e.to_string())?;
    let user = purgatorio::auth::user();
    let proof = purgatorio::auth::fmt::proof::make(&[0x55; 32], None, &signed, &user, "");
    want_err(
        f.write_all(&proof).map_err(Error::from),
        purgatorio::abi::auth::FAILED,
    )?;
    drop(f);
    let r = purgatorio::mount(conn.as_raw_fd(), -1, &dir, MREPL, "");
    want_err(r, purgatorio::abi::auth::ENCRYPTED)
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

/// A prog blocked in stream is killed like one blocked in a read.
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
    // The kill ends the prog, after what it copied has reached the file.
    if msg.status != "killed" {
        return Err(format!("wait {msg:?}"));
    }
    if std::fs::read(path).ok().as_deref() != Some(&b"abc"[..]) {
        return Err(String::from("the copy is lost"));
    }
    Ok(())
}
