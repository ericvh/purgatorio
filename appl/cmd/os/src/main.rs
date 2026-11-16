//! os [-b] [-m mountpoint] [-d dir] [-n] [-N level] cmd [arg ...]: run a
//! command on the host through cmd(3), as Inferno's os(1).
//!
//! `os` uses the `#C` device at `mountpoint` (default `/cmd`), binding it
//! there when it is not. `-d` runs the command in host directory `dir`,
//! and `-n` (or `-N` with a level from 1 to 3) at lower priority. The
//! command's standard output and error appear on `os`'s own; `os` copies
//! its standard input to the command's (redirect it from `/dev/null` when
//! there is none), and the command's input ends when `os`'s does. `os`
//! ends when the command does, with its exit status. If `os` is killed or
//! exits first, the command is killed, unless `-b` was given.

use purgatorio::eprintln;
use purgatorio::fd::AsRawFd;
use purgatorio::fs::File;
use std::io::{Read, Write};

use purgatorio::abi::mode::{ORDWR, OREAD, OWRITE};
use purgatorio::abi::mount::MREPL;
use purgatorio::abi::pctl::NEWFD;
use purgatorio::arg::Arg;
use purgatorio::env::{quote_list, unquote_list};
use purgatorio::{Context, Error};

const USAGE: &str = "os [-b] [-m mountpoint] [-d dir] [-n] [-N level] cmd [arg ...]";
const SELF: &str = "/bin/os.wasm";
/// The helper mode: copy standard input to standard output, then exit.
const COPY: &str = "--copy";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some(COPY) {
        let _ = purgatorio::stream(0, 1, 8192);
        return Ok(());
    }
    let mut arg = Arg::new(args, USAGE);
    let (mut background, mut mnt, mut dir, mut nice) = (false, String::from("/cmd"), None, 0u8);
    while let Some(c) = arg.opt() {
        match c {
            'b' => background = true,
            'm' => mnt = arg.earg()?,
            'd' => dir = Some(arg.earg()?),
            'n' => nice = 1,
            'N' => {
                nice = arg
                    .earg()?
                    .parse()
                    .ok()
                    .filter(|n| (1..=3).contains(n))
                    .ok_or_else(|| arg.usage())?;
            }
            _ => return Err(arg.usage()),
        }
    }
    let argv = arg.argv();
    if argv.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let status = run(&mnt, dir.as_deref(), nice, background, &argv)?;
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}

/// Writes a control message to `ctl`, in one write.
fn ctl(mut fd: &File, msg: &str) -> Result<(), Error> {
    fd.write(msg.as_bytes()).map(drop).map_err(Error::from)
}

/// Runs `argv` on the host and returns its exit status.
fn run(
    mnt: &str,
    dir: Option<&str>,
    nice: u8,
    background: bool,
    argv: &[String],
) -> Result<String, Error> {
    if purgatorio::stat(&format!("{mnt}/clone")).is_err() {
        purgatorio::bind("#C", mnt, MREPL)
            .map_err(|e| Error(format!("cannot bind #C on {mnt}: {e}")))?;
    }
    let mut clone = File::from(
        purgatorio::open(&format!("{mnt}/clone"), ORDWR)
            .map_err(|e| Error(format!("cannot open {mnt}/clone: {e}")))?,
    );
    let mut idbuf = [0u8; 32];
    let n = clone
        .read(&mut idbuf)
        .map_err(|e| Error(format!("cannot read {mnt}/clone: {}", Error::from(e))))?;
    let id = std::str::from_utf8(&idbuf[..n])
        .unwrap_or("")
        .trim()
        .to_string();
    let conv = format!("{mnt}/{id}");
    if let Some(d) = dir {
        ctl(&clone, &format!("dir {}", quote_list(&[d.to_string()]))).map_err(|e| {
            Error(format!(
                "cannot set cwd {}: {e}",
                quote_list(&[d.to_string()])
            ))
        })?;
    }
    if nice > 0
        && let Err(e) = ctl(&clone, &format!("nice {nice}"))
    {
        eprintln!("os: warning: can't set nice priority: {e}");
    }
    // cmd(3): wait must be open before the command starts.
    let mut wait = File::from(purgatorio::open(&format!("{conv}/wait"), OREAD)?);
    if !background && let Err(e) = ctl(&clone, "killonclose") {
        eprintln!("os: warning: cannot write killonclose: {e}");
    }
    ctl(&clone, &format!("exec {}", quote_list(argv)))
        .map_err(|e| Error(format!("cannot exec: {e}")))?;
    let data_w = purgatorio::open(&format!("{conv}/data"), OWRITE)?;
    let data_r = purgatorio::open(&format!("{conv}/data"), OREAD)?;
    let err_r = purgatorio::open(&format!("{conv}/stderr"), OREAD)?;
    let copy = |from: i32, to: i32| {
        purgatorio::spawn(
            SELF,
            &[String::from("os"), String::from(COPY)],
            NEWFD,
            &[(from, 0), (to, 1)],
        )
    };
    // This prog's input to the command's; the helper's exit, at the end
    // of the input, closes the command's.
    let feeder = copy(0, data_w.as_raw_fd())?;
    drop(data_w);
    let errors = copy(err_r.as_raw_fd(), 2)?;
    drop(err_r);
    let _ = purgatorio::stream(data_r.as_raw_fd(), 1, 8192);
    drop(data_r);
    let mut line = [0u8; 256];
    let n = wait.read(&mut line)?;
    let fields = unquote_list(std::str::from_utf8(&line[..n]).unwrap_or(""));
    // pid user sys real status; a non-empty status is the host's.
    let status = match fields.get(4) {
        Some(s) if !s.is_empty() => format!("host: {s}"),
        _ => String::new(),
    };
    // The feeder may be waiting for input that never comes.
    let _ = purgatorio::fs::write(format!("/prog/{feeder}/ctl"), "kill");
    let mut left = 2;
    while left > 0 {
        match purgatorio::wait() {
            Ok(w) if w.pid == feeder || w.pid == errors => left -= 1,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(status)
}
