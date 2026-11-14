//! m4: the macro processor of Inferno's m4(1), close to Seventh Edition
//! m4.

mod eval;
mod m4;

use std::io::{Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

use crate::m4::{Fatal, Host, M4};

const USAGE: &str = "m4 [-t] [-pprefix] [-Dname[=value]] [-Qname[=value]] [-Uname] [file ...]";

const SH: &str = "/bin/sh.wasm";

/// The system behind the processor: buffered standard output, files, sh.
struct Sys {
    out: std::io::BufWriter<std::io::Stdout>,
}

impl Host for Sys {
    fn out(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn err(&mut self, s: &str) {
        let _ = std::io::stderr().write_all(s.as_bytes());
    }

    fn open(&mut self, path: &str) -> Result<Box<dyn Read>, String> {
        match std::fs::File::open(path) {
            Ok(f) => Ok(Box::new(f)),
            Err(e) => Err(Error::from(e).0),
        }
    }

    fn syscmd(&mut self, cmd: &str) -> Result<(), String> {
        let argv = ["sh", "-c", cmd].map(String::from);
        let pid = purgatorio::spawn(SH, &argv, 0, &[])
            .map_err(|e| format!("load: can't load {SH}: {e}"))?;
        while let Ok(w) = purgatorio::wait() {
            if w.pid == pid {
                break;
            }
        }
        Ok(())
    }

    fn exists(&mut self, path: &str) -> bool {
        purgatorio::stat(path).is_ok()
    }

    fn sysname(&mut self) -> Option<String> {
        std::fs::read("/dev/sysname")
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn pid(&mut self) -> u32 {
        purgatorio::pid()
    }
}

/// Prints a fatal error and exits as Inferno's m4 does.
fn die(f: Fatal) -> ! {
    eprintln!("{}", f.0);
    purgatorio::exits("fail:error");
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut prefix = String::new();
    let mut tracing = false;
    let mut defs = Vec::new();
    while let Some(c) = arg.opt() {
        match c {
            'D' | 'Q' | 'U' => defs.push((c, arg.earg()?)),
            'p' => prefix = arg.earg()?,
            't' => tracing = true,
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    let sys = Sys {
        out: std::io::BufWriter::new(std::io::stdout()),
    };
    let mut m = M4::new(sys, &prefix, tracing);
    for (c, v) in defs {
        match c {
            'U' => m.undefine(&v),
            c => m.argdefine(&v, c == 'Q').unwrap_or_else(|f| die(f)),
        }
    }
    if files.is_empty() {
        m.pushfile("standard input", Box::new(std::io::stdin()));
        m.scan().unwrap_or_else(|f| die(f));
    }
    for f in &files {
        match m.host.open(f) {
            Ok(r) => m.pushfile(f, r),
            Err(e) => die(m.error(&format!("can't open {f}: {e}"))),
        }
        m.scan().unwrap_or_else(|f| die(f));
    }
    m.finish();
    Ok(())
}
