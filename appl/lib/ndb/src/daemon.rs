//! Servers that put a file in the namespace and run on in the background,
//! as Inferno's `ndb/cs` and `ndb/dns` do: the command starts a copy of
//! itself with `--serve` and returns once the copy says it is ready.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;

use purgatorio::Error;
use purgatorio::abi::pctl;

/// The flag that marks the serving copy.
pub const SERVE: &str = "--serve";

/// Whether `args` are the serving copy's; if so the flag is removed.
pub fn serving(args: &mut Vec<String>) -> bool {
    if args.get(1).map(String::as_str) == Some(SERVE) {
        args.remove(1);
        true
    } else {
        false
    }
}

/// Starts `module` with `args` (argv\[0\] first) and `--serve`, and waits
/// for [`ready`]. `stderr` gives the server this prog's standard error.
pub fn start(module: &str, args: &[String], stderr: bool) -> Result<u32, Error> {
    let [r, w] = purgatorio::pipe()?;
    let mut child = vec![args[0].clone(), String::from(SERVE)];
    child.extend(args.iter().skip(1).cloned());
    let mut fds = vec![(w.as_raw_fd(), 1)];
    if stderr {
        fds.push((2, 2));
    }
    let pid = purgatorio::spawn(module, &child, pctl::NEWFD, &fds)?;
    drop(w);
    // One read: the server keeps its end open while it serves.
    let mut buf = [0u8; 512];
    let n = File::from(r).read(&mut buf)?;
    let reply = String::from_utf8_lossy(&buf[..n]).into_owned();
    match reply.strip_prefix('R') {
        Some(_) => Ok(pid),
        None if reply.is_empty() => Err(Error::from("server failed to start")),
        None => Err(Error(reply)),
    }
}

/// Tells the starting command that the server is ready (`Ok`) or why it
/// is not.
pub fn ready(r: Result<(), &str>) {
    let msg = match r {
        Ok(()) => "R",
        Err(e) => e,
    };
    let _ = purgatorio::file2chan::write_fd(1, msg.as_bytes());
}

/// Serves `name` in `dir`. On `#I`, which cannot hold new files, the file
/// is made in `#s` and `#s` is bound before `dir`, as Inferno's servers do.
pub fn file2chan(dir: &str, name: &str) -> Result<purgatorio::file2chan::FileIO, Error> {
    let on_ip = purgatorio::stat(dir).is_ok_and(|d| d.dtype == u16::from(b'I'));
    let exists = purgatorio::stat(&format!("{dir}/{name}")).is_ok();
    if on_ip && !exists {
        let io = purgatorio::file2chan::file2chan("#s", name)?;
        purgatorio::bind("#s", dir, purgatorio::abi::mount::MBEFORE)?;
        return Ok(io);
    }
    purgatorio::file2chan::file2chan(dir, name)
}
