//! The helpers `auxi/rstyxd` starts as itself.

use std::io::{Read, Write};

use purgatorio::abi::mode::OREAD;
use purgatorio::abi::mount::{MNOAUTH, MREPL};
use purgatorio::abi::pctl::NEWFD;
use purgatorio::{DMDIR, Error, auth};
use rstyx::io::Fd;
use rstyx::proto;

/// Descriptors the helpers are given, past the ones WASI reserves (3 and
/// 4 are the preopened `.` and `/`).
pub const ACK: i32 = 5;
pub const STYX: i32 = 5;
pub const STATUS: i32 = 6;

/// Where the client's namespace is mounted.
const CLIENT: &str = "/n/client";

/// `--feed`: copy fd 0 to fd 1 and, after each piece is written, write
/// its length (u32, little-endian) to fd [`ACK`]. A command that does not read
/// its input blocks this prog, not the one carrying the connection.
pub fn feed() -> Result<(), Error> {
    let mut buf = vec![0u8; proto::CHUNK];
    loop {
        let n = match Fd(0).read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ => return Ok(()),
        };
        if Fd(1).write_all(&buf[..n]).is_err()
            || Fd(ACK).write_all(&(n as u32).to_le_bytes()).is_err()
        {
            return Ok(());
        }
    }
}

fn mkdir(path: &str) {
    if purgatorio::stat(path).is_err() {
        let _ = purgatorio::create(path, OREAD, DMDIR | 0o777);
    }
}

/// Mounts the client's namespace (the 9P conversation on fd [`STYX`]) and runs
/// `cmd`, returning its exit status.
fn command(flags: u8, keyfile: &str, cmd: &str) -> Result<String, Error> {
    mkdir("/n");
    mkdir(CLIENT);
    let mount = if flags & proto::NOAUTH != 0 {
        purgatorio::mount(STYX, -1, CLIENT, MREPL | MNOAUTH, "")
    } else if !keyfile.is_empty() || auth::have_factotum() {
        let keyfile = Some(keyfile).filter(|k| !k.is_empty());
        auth::amount_key(STYX, CLIENT, MREPL, "", keyfile)
    } else {
        // The kernel authenticates when the client's export asks.
        purgatorio::mount(STYX, -1, CLIENT, MREPL, "")
    };
    mount.map_err(|e| Error(format!("mount {CLIENT}: {e}")))?;
    let argv = ["sh", "-c", cmd].map(String::from);
    let pid = purgatorio::spawn("/bin/sh.wasm", &argv, NEWFD, &[(0, 0), (1, 1), (2, 2)])
        .map_err(|e| Error(format!("sh: {e}")))?;
    loop {
        let w = purgatorio::wait()?;
        if w.pid == pid {
            return Ok(w.status);
        }
    }
}

/// `--session flags keyfile cmd`: run the command with fds 0 to 2 as it was
/// given them, write `s` and its exit status to fd [`STATUS`], and stay until
/// killed, so that this prog's process group can always be named.
pub fn run(args: &[String]) -> Result<(), Error> {
    let [flags, keyfile, cmd] = args else {
        return Err(Error::from("usage: auxi/rstyxd [-A] [-k keyfile]"));
    };
    let status = match command(flags.parse().unwrap_or(0), keyfile, cmd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("auxi/rstyxd: {e}");
            format!("fail:{e}")
        }
    };
    // The command's descriptors are not held past its exit.
    for fd in [0, 1, 2, STYX] {
        // SAFETY: nothing in this prog uses these descriptors again.
        drop(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    }
    Fd(STATUS).write_all(format!("s{status}").as_bytes())?;
    let mut b = [0u8; 1];
    let _ = Fd(STATUS).read(&mut b);
    Ok(())
}
