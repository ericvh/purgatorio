//! disk/kfscmd [-n name] cmd ...: send each `cmd` to the console of the
//! `disk/kfs` started with `-n name` (default `main`),
//! `/chan/kfs.<name>.cmd`, and copy what it answers to standard output,
//! as kfscmd(8). The commands are `allow`, `allowoff` (or `disallow`),
//! `check [cdfpPqrtw]`, `sync` and `users`; a command of several words
//! is one argument.

use std::io::{Read, Write};

use purgatorio::arg::Arg;
use purgatorio::ns::quoted;
use purgatorio::{Context, Error};

const USAGE: &str = "disk/kfscmd [-n fsname] cmd ...";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // As Inferno's kfscmd: any failure is `fail:error`.
    run(args).map_err(|e| e.fails_as("fail:error"))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut fs = String::from("main");
    while let Some(c) = arg.opt() {
        match c {
            'n' => fs = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let path = format!("/chan/kfs.{fs}.cmd");
    let mut ctl = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| Error(format!("can't open {path}: {}", Error::from(e))))?;
    let mut out = std::io::stdout().lock();
    for cmd in arg.argv() {
        if let Err(e) = ctl.write(cmd.as_bytes()) {
            return Err(Error(format!("{}: {}", quoted(&cmd), Error::from(e))));
        }
        let mut buf = [0u8; 1024];
        loop {
            let n = ctl.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
    }
    Ok(())
}
