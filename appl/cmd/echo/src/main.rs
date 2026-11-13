//! echo [-n] [arg ...]: write the arguments, separated by spaces and followed
//! by a newline unless -n is given, in a single write.
//!
//! As in Inferno, the line goes to fd 1 in one write, not through
//! `std::io::stdout`: its buffer keeps what a failed write did not take and
//! writes it again at exit, so the file would see the data twice. A failed
//! or short write is reported as `echo: write error: ...` with the status
//! `fail:write error`, and is not retried.

use std::fs::File;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut words = &args[1.min(args.len())..];
    let newline = words.first().map(String::as_str) != Some("-n");
    if !newline {
        words = &words[1..];
    }
    let mut out = words.join(" ");
    if newline {
        out.push('\n');
    }
    if out.is_empty() {
        return Ok(());
    }
    // SAFETY: fd 1 outlives main; ManuallyDrop leaves it open.
    let mut stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let err = match stdout.write(out.as_bytes()) {
        Ok(n) if n == out.len() => return Ok(()),
        Ok(_) => String::from("short write"),
        Err(_) => purgatorio::errstr(),
    };
    eprintln!("echo: write error: {err}");
    purgatorio::exits("fail:write error")
}
