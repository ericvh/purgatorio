//! `dial`, `announce` and `listen`, as file operations on `/net`.
//!
//! An address is `net!host!port`, `host!port` (TCP) or a bare port
//! (`tcp!*!port`). `/net/cs` translates names; `/net/tcp/clone` opens a
//! conversation. The kernel's `/net/cs` answers `tcp!10.0.0.2!80`; the
//! guest connection server, `ndb/cs`, answers as Inferno's does, one
//! `/net/tcp/clone 10.0.0.2!80` line per address, and [`dial`] tries each
//! in turn.

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

/// Connects to `addr` and returns the conversation. Each translation
/// `/net/cs` gives is tried in turn; the last failure is returned.
pub fn dial(addr: &str) -> Result<Connection, Error> {
    let mut last = Error::from("cs: can't translate address");
    for t in translate(&netmkaddr(addr))? {
        match dial_translated(&t) {
            Ok(c) => return Ok(c),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Connects to `net!host!port` without asking `/net/cs`: `host` must be
/// numeric. A server that `/net/cs` itself consults (`ndb/dns`) dials this
/// way.
pub fn dial_translated(addr: &str) -> Result<Connection, Error> {
    let (net, host, port) = three(addr)?;
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

/// Asks `/net/cs` for the translations of `addr`, as `net!host!port`.
pub fn translate(addr: &str) -> Result<Vec<String>, Error> {
    let mut cs = open_rw("/net/cs")?;
    cs.write_all(addr.as_bytes()).map_err(io)?;
    cs.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut buf = String::new();
    cs.read_to_string(&mut buf).map_err(io)?;
    let out: Vec<String> = buf.lines().filter_map(cs_line).collect();
    if out.is_empty() {
        Err(Error::from("cs: can't translate address"))
    } else {
        Ok(out)
    }
}

/// One line of a `/net/cs` reply as `net!host!port`: either that already
/// (the kernel's), or Inferno's `/net/tcp/clone host!port`.
pub fn cs_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Some((clone, addr)) = line.split_once(' ') else {
        return Some(line.to_string());
    };
    let net = clone.strip_suffix("/clone")?.rsplit('/').next()?;
    let addr = addr.trim();
    Some(match addr.split_once('!') {
        Some(_) => format!("{net}!{addr}"),
        None => format!("{net}!*!{addr}"),
    })
}

/// One read of at most `max` bytes from `fd`, a UDP conversation's data
/// file say, that gives up after `ms` milliseconds: `Ok(None)` then.
///
/// A read on `#I` has no timeout of its own, so, as Inferno's commands do
/// with a timer process, the read is made by a child prog (`read`) that a
/// second one (`sh -c 'sleep; kill'`) kills when the time is up.
pub fn read_timeout(fd: i32, max: usize, ms: u64) -> Result<Option<Vec<u8>>, Error> {
    let [r, w] = crate::pipe()?;
    let reader = crate::spawn(
        "/bin/read.wasm",
        &[String::from("read"), max.to_string()],
        crate::abi::pctl::NEWFD,
        &[(fd, 0), (std::os::fd::AsRawFd::as_raw_fd(&w), 1)],
    )?;
    drop(w);
    let alarm = alarm(reader, ms)?;
    let mut buf = Vec::new();
    let got = File::from(r).read_to_end(&mut buf);
    drop(alarm);
    got.map_err(io)?;
    Ok(if buf.is_empty() { None } else { Some(buf) })
}

/// Kills prog `pid` after `ms` milliseconds unless dropped first.
pub struct Alarm(u32);

/// Starts an [`Alarm`].
pub fn alarm(pid: u32, ms: u64) -> Result<Alarm, Error> {
    let script = format!(
        "sleep {}.{:03}; echo kill > /prog/{pid}/ctl",
        ms / 1000,
        ms % 1000
    );
    // sh wants standard files; give it /dev/null.
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(io)?;
    let n = std::os::fd::AsRawFd::as_raw_fd(&null);
    let id = crate::spawn(
        "/bin/sh.wasm",
        &[String::from("sh"), String::from("-c"), script],
        crate::abi::pctl::NEWFD,
        &[(n, 0), (n, 1), (n, 2)],
    )?;
    Ok(Alarm(id))
}

impl Drop for Alarm {
    fn drop(&mut self) {
        // The alarm may have gone off and exited already.
        let _ = std::fs::write(format!("/prog/{}/ctl", self.0), "kill");
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
