//! cat [file ...]: copy each file, or standard input if none, to standard
//! output.
//!
//! Files are opened with plain `std::fs::File`, which goes through WASI;
//! failures are reported with the exact Inferno error string from
//! `purgatorio::errstr`. Each read is written to fd 1 as it is, as
//! Inferno's cat does: not through `std::io::stdout`, whose line
//! buffering searches every buffer for a newline and copies small writes.

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

use purgatorio::{Context, Error};

fn copy(from: &mut dyn Read, out: &mut dyn Write) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n])?;
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // SAFETY: fd 1 outlives main; ManuallyDrop leaves it open.
    let mut stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let stdout: &mut File = &mut stdout;
    if args.len() <= 1 {
        copy(&mut io::stdin().lock(), stdout)?;
        return Ok(());
    }
    let mut failed = false;
    for path in &args[1..] {
        let res = File::open(path).and_then(|mut f| copy(&mut f, stdout));
        if res.is_err() {
            eprintln!("cat: {path}: {}", purgatorio::errstr());
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
