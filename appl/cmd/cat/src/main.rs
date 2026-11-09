//! cat [file ...]: copy each file, or standard input if none, to standard
//! output.
//!
//! Files are opened with plain `std::fs::File`, which goes through WASI;
//! failures are reported with the exact Inferno error string from
//! `purgatorio::errstr`.

use std::fs::File;
use std::io::{self, Read, Write};

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
    let mut stdout = io::stdout().lock();
    if args.len() <= 1 {
        copy(&mut io::stdin().lock(), &mut stdout)?;
        stdout.flush()?;
        return Ok(());
    }
    let mut failed = false;
    for path in &args[1..] {
        let res = File::open(path).and_then(|mut f| copy(&mut f, &mut stdout));
        if res.is_err() {
            let _ = stdout.flush();
            eprintln!("cat: {path}: {}", purgatorio::errstr());
            failed = true;
        }
    }
    stdout.flush()?;
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
