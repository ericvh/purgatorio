//! net: dial, announce, an export mounted over TCP, and `#C`.
//!
//! `net --accept dir` accepts one connection and serves `/` on it, then
//! waits for another that never comes. `net --udp addr` checks a UDP echo
//! server. With no arguments the checks run.
//! A platform with no network prints `ok net_skip` and still succeeds.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};

use purgatorio::abi::mount::MREPL;
use purgatorio::abi::pctl::NEWFD;
use purgatorio::dial::{self, local};
use purgatorio::{Context, Error};

const SELF: &str = "/bin/test/net.wasm";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--accept") => accept(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--wasi-dial") => wasi_dial(args.get(2).map(String::as_str).unwrap_or("")),
        Some("--udp") => udp_echo(args.get(2).map(String::as_str).unwrap_or("")),
        _ => checks(),
    }
}

#[link(wasm_import_module = "wasi_snapshot_preview1")]
unsafe extern "C" {
    fn sock_accept(fd: i32, flags: i32, result: *mut i32) -> i32;
}

fn wasi_dial(addr: &str) -> Result<(), Error> {
    let conn = dial::dial(addr)?;
    let mut data = open_data(&conn.dir)?;
    data.write_all(b"ping")?;
    Ok(())
}

/// Accepts one connection with WASI `sock_accept` on the Inferno listen file.
fn wasi_accept(ann: &dial::Connection) -> Result<(), Error> {
    let here = local(&ann.dir)?;
    purgatorio::spawn(SELF, &[SELF.into(), "--wasi-dial".into(), here], 0, &[])?;
    let listen = std::fs::File::open(format!("{}/listen", ann.dir))?;
    let mut got = -1i32;
    let rc = unsafe { sock_accept(listen.as_raw_fd(), 0, &mut got) };
    if rc != 0 {
        return Err(Error(format!("sock_accept errno {rc}")));
    }
    let mut data = unsafe { std::fs::File::from_raw_fd(got) };
    let mut buf = [0u8; 4];
    data.read_exact(&mut buf)?;
    if &buf != b"ping" {
        return Err(Error(format!("sock_accept got {buf:?}")));
    }
    Ok(())
}

fn accept(dir: &str) -> Result<(), Error> {
    let conn = dial::listen(dir)?;
    let Some(data) = conn.data.as_ref() else {
        return Err(Error::from("no data"));
    };
    purgatorio::spawn(
        "/bin/export.wasm",
        &["export".into(), "/".into()],
        NEWFD,
        &[(data.as_raw_fd(), 0)],
    )?;
    // Park until the caller is done. The second listen never completes.
    let _ = dial::listen(dir);
    Ok(())
}

fn checks() -> Result<(), Error> {
    cs()?;
    println!("ok cs");
    match os_echo() {
        Ok(()) => println!("ok os"),
        Err(e) if skippable(&e) => println!("ok os_skip"),
        Err(e) => return Err(e),
    }
    let ann = match dial::announce("tcp!127.0.0.1!0") {
        Ok(ann) => ann,
        Err(e) if skippable(&e) => {
            println!("ok net_skip");
            return Ok(());
        }
        Err(e) => return Err(Error(format!("announce: {e}"))),
    };
    echo(&ann)?;
    println!("ok dial");
    udp()?;
    println!("ok udp");
    wasi_accept(&ann)?;
    println!("ok sock_accept");
    export_mount()?;
    println!("ok export");
    Ok(())
}

fn echo(ann: &dial::Connection) -> Result<(), Error> {
    let here = local(&ann.dir)?;
    let port = here.rsplit('!').next().unwrap_or("0");
    let client = dial::dial(&format!("tcp!127.0.0.1!{port}"))?;
    let server = dial::listen(&ann.dir)?;
    let mut out = open_data(&client.dir)?;
    out.write_all(b"ping")?;
    let mut data = open_data(&server.dir)?;
    let mut buf = [0u8; 4];
    data.read_exact(&mut buf)?;
    if &buf != b"ping" {
        return Err(Error::from("echo mismatch"));
    }
    Ok(())
}

/// UDP over loopback: the first datagram from a new remote starts a
/// conversation on the announced port, and each read or write is one
/// datagram.
fn udp() -> Result<(), Error> {
    let ann = dial::announce("udp!127.0.0.1!0").map_err(|e| Error(format!("udp: {e}")))?;
    let here = local(&ann.dir)?;
    let port = here.rsplit('!').next().unwrap_or("0");
    let client = dial::dial(&format!("udp!127.0.0.1!{port}"))?;
    let mut out = open_data(&client.dir)?;
    out.write_all(b"ping")?;
    out.write_all(b"again")?;
    let server = dial::listen(&ann.dir)?;
    if local(&client.dir)? != remote(&server.dir)? {
        return Err(Error::from(
            "udp: the conversation's remote is not the dialer",
        ));
    }
    let mut data = open_data(&server.dir)?;
    let mut buf = [0u8; 64];
    for want in [&b"ping"[..], b"again"] {
        let n = data.read(&mut buf)?;
        if &buf[..n] != want {
            return Err(Error(format!("udp: got {:?}", &buf[..n])));
        }
    }
    data.write_all(b"pong")?;
    let n = out.read(&mut buf)?;
    if &buf[..n] != b"pong" {
        return Err(Error(format!("udp: reply {:?}", &buf[..n])));
    }
    Ok(())
}

/// `net --udp addr`: sends `ping` to a UDP echo server and checks the echo.
fn udp_echo(addr: &str) -> Result<(), Error> {
    let conn = dial::dial(addr).map_err(|e| Error(format!("udp: {e}")))?;
    let mut data = open_data(&conn.dir)?;
    data.write_all(b"ping")?;
    let mut buf = [0u8; 64];
    let n = data.read(&mut buf)?;
    if &buf[..n] != b"ping" {
        return Err(Error(format!("udp: echo {:?}", &buf[..n])));
    }
    println!("ok udp_echo");
    Ok(())
}

fn remote(dir: &str) -> Result<String, Error> {
    Ok(std::fs::read_to_string(format!("{dir}/remote"))?
        .trim()
        .to_string())
}

fn export_mount() -> Result<(), Error> {
    let ann = dial::announce("tcp!127.0.0.1!0")?;
    let here = local(&ann.dir)?;
    let port = here.rsplit('!').next().unwrap_or("0");
    std::fs::create_dir_all("/tmp/netmnt")?;
    purgatorio::spawn(
        SELF,
        &[SELF.into(), "--accept".into(), ann.dir.clone()],
        0,
        &[],
    )
    .map_err(|e| Error(format!("spawn: {e}")))?;
    let client =
        dial::dial(&format!("tcp!127.0.0.1!{port}")).map_err(|e| Error(format!("dial: {e}")))?;
    let fd = client
        .data
        .as_ref()
        .ok_or_else(|| Error::from("no data"))?
        .as_raw_fd();
    purgatorio::mount(fd, -1, "/tmp/netmnt", MREPL, "")
        .map_err(|e| Error(format!("mount: {e}")))?;
    let text = std::fs::read_to_string("/tmp/netmnt/lib/probe/hello.txt")
        .map_err(|e| Error(format!("read: {e}")))?;
    if !text.contains("hello") {
        return Err(Error(format!("unexpected export: {text:?}")));
    }
    Ok(())
}

fn cs() -> Result<(), Error> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/net/cs")?;
    file.write_all(b"tcp!127.0.0.1!80")?;
    file.seek(SeekFrom::Start(0))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if !buf.contains("127.0.0.1!80") {
        return Err(Error(format!("cs: {buf:?}")));
    }
    Ok(())
}

fn os_echo() -> Result<(), Error> {
    let mut clone = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/cmd/clone")?;
    let mut idbuf = [0u8; 32];
    let n = clone.read(&mut idbuf)?;
    let id = std::str::from_utf8(&idbuf[..n]).unwrap_or("").trim();
    clone.write_all(b"echo hello-os")?;
    let mut data = std::fs::File::open(format!("/cmd/{id}/data"))?;
    let mut out = String::new();
    data.read_to_string(&mut out)?;
    if out.trim() != "hello-os" {
        return Err(Error(format!("os: {out:?}")));
    }
    Ok(())
}

fn open_data(dir: &str) -> Result<std::fs::File, Error> {
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{dir}/data"))?)
}

fn skippable(e: &Error) -> bool {
    let msg = e.to_string();
    msg.contains("network unreachable")
        || msg.contains("not implemented")
        || msg.contains("file does not exist")
}
