//! `dial`, `announce` and `listen`, as file operations on `/net`.
//!
//! An address is `net!host!port`, `host!port` (TCP) or a bare port
//! (`tcp!*!port`). `/net/cs` translates names; `/net/tcp/clone` opens a
//! conversation.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

use crate::Error;

/// A `/net` conversation. `data` is the byte stream of a connected
/// conversation; a listener leaves it empty.
pub struct Connection {
    pub dir: String,
    pub ctl: OwnedFd,
    pub data: Option<OwnedFd>,
}

/// Fills in a missing network or host. `80` becomes `tcp!*!80`.
pub fn netmkaddr(addr: &str) -> String {
    match addr.split('!').collect::<Vec<_>>().as_slice() {
        [port] => format!("tcp!*!{port}"),
        [host, port] => format!("tcp!{host}!{port}"),
        _ => addr.to_string(),
    }
}

/// Connects to `addr` and returns the conversation.
pub fn dial(addr: &str) -> Result<Connection, Error> {
    let addr = translate(&netmkaddr(addr))?;
    let (net, host, port) = three(&addr)?;
    let (id, mut ctl) = clone(&net)?;
    write_ctl(&mut ctl, &format!("connect {host}!{port}"))?;
    let data = open_rw(&format!("/net/{net}/{id}/data"))?;
    Ok(Connection {
        dir: format!("/net/{net}/{id}"),
        ctl: owned(ctl),
        data: Some(owned(data)),
    })
}

/// Announces `addr` (`tcp!*!0` asks for a free port) and returns the listener.
pub fn announce(addr: &str) -> Result<Connection, Error> {
    let addr = netmkaddr(addr);
    let (net, host, port) = three(&addr)?;
    let (id, mut ctl) = clone(&net)?;
    write_ctl(&mut ctl, &format!("announce {host}!{port}"))?;
    Ok(Connection {
        dir: format!("/net/{net}/{id}"),
        ctl: owned(ctl),
        data: None,
    })
}

/// Accepts one connection on a listener's directory (`/net/tcp/0`).
pub fn listen(dir: &str) -> Result<Connection, Error> {
    let mut listen = File::open(format!("{dir}/listen")).map_err(io)?;
    let mut buf = [0u8; 64];
    let n = listen.read(&mut buf).map_err(io)?;
    if n == 0 {
        return Err(Error::from("listen: hangup"));
    }
    let id = std::str::from_utf8(&buf[..n]).unwrap_or("").trim();
    let parent = dir.rsplit_once('/').map(|(p, _)| p).unwrap_or("/net/tcp");
    let dir = format!("{parent}/{id}");
    let ctl = open_rw(&format!("{dir}/ctl"))?;
    let data = open_rw(&format!("{dir}/data"))?;
    Ok(Connection {
        dir,
        ctl: owned(ctl),
        data: Some(owned(data)),
    })
}

/// The `local` file of a conversation, such as `127.0.0.1!1234` or `*!1234`.
pub fn local(dir: &str) -> Result<String, Error> {
    let text = std::fs::read_to_string(format!("{dir}/local")).map_err(io)?;
    Ok(text.trim().to_string())
}

fn translate(addr: &str) -> Result<String, Error> {
    let mut cs = open_rw("/net/cs")?;
    cs.write_all(addr.as_bytes()).map_err(io)?;
    cs.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut buf = String::new();
    cs.read_to_string(&mut buf).map_err(io)?;
    let line = buf.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        Err(Error::from("cs: no translation"))
    } else {
        Ok(line.to_string())
    }
}

fn clone(net: &str) -> Result<(String, File), Error> {
    let mut ctl = open_rw(&format!("/net/{net}/clone"))?;
    let mut buf = [0u8; 32];
    let n = ctl.read(&mut buf).map_err(io)?;
    let id = std::str::from_utf8(&buf[..n])
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        return Err(Error::from("clone: no conversation"));
    }
    Ok((id, ctl))
}

fn write_ctl(ctl: &mut File, cmd: &str) -> Result<(), Error> {
    ctl.write_all(cmd.as_bytes()).map_err(io)
}

fn open_rw(path: &str) -> Result<File, Error> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io)
}

fn three(addr: &str) -> Result<(String, String, String), Error> {
    let p: Vec<&str> = addr.split('!').collect();
    if p.len() < 3 {
        return Err(Error::from("bad address"));
    }
    Ok((p[0].to_string(), p[1].to_string(), p[2].to_string()))
}

fn owned(file: File) -> OwnedFd {
    // SAFETY: `file` owns the descriptor, and this takes that ownership.
    unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) }
}

fn io(e: std::io::Error) -> Error {
    let err = crate::errstr();
    if err.is_empty() {
        Error::from(e.to_string())
    } else {
        Error::from(err)
    }
}
