//! bench fs NAME DIR N | bench spawn NAME N: micro-benchmarks for
//! `cargo xtask bench` (`docs/perf.md`). Each prints `bench NAME-STEP
//! SECONDS` lines, which `xtask` collects.
//!
//! - `fs NAME DIR N`: creates N files of 64 bytes in `DIR` (create, write,
//!   close), reads each back (open, read, close), stats each, then
//!   removes them. One line per step.
//! - `spawn NAME N`: spawns `/bin/echo.wasm -n` N times, waiting for each.
//!   The line gives the total.

use std::io::{Read, Write};
use std::time::Instant;

use purgatorio::abi::mode;
use purgatorio::{Context, Error};

const USAGE: &str = "usage: bench fs name dir n | bench spawn name n";

fn count(s: Option<&String>) -> Result<usize, Error> {
    s.and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::from(USAGE))
}

fn report(name: &str, step: &str, t: Instant) {
    println!("bench {name}-{step} {:.6}", t.elapsed().as_secs_f64());
}

fn fs(name: &str, dir: &str, n: usize) -> Result<(), Error> {
    let data = [b'x'; 64];
    let path = |i: usize| format!("{dir}/f{i}");
    let t = Instant::now();
    for i in 0..n {
        let fd = purgatorio::create(&path(i), mode::OWRITE, 0o644)?;
        std::fs::File::from(fd).write_all(&data)?;
    }
    report(name, "create", t);
    let t = Instant::now();
    let mut buf = [0u8; 128];
    for i in 0..n {
        let fd = purgatorio::open(&path(i), mode::OREAD)?;
        let got = std::fs::File::from(fd).read(&mut buf)?;
        if got != data.len() {
            return Err(Error::from(format!("{}: read {got} bytes", path(i))));
        }
    }
    report(name, "read", t);
    let t = Instant::now();
    for i in 0..n {
        purgatorio::stat(&path(i))?;
    }
    report(name, "stat", t);
    let t = Instant::now();
    for i in 0..n {
        purgatorio::remove(&path(i))?;
    }
    report(name, "remove", t);
    Ok(())
}

fn spawn(name: &str, n: usize) -> Result<(), Error> {
    let argv = [String::from("echo"), String::from("-n")];
    let t = Instant::now();
    for _ in 0..n {
        let pid = purgatorio::spawn("/bin/echo.wasm", &argv, 0, &[])?;
        let w = purgatorio::wait()?;
        if w.pid != pid || !w.status.is_empty() {
            return Err(Error::from(format!("spawn: {w:?}")));
        }
    }
    report(name, "total", t);
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("fs"), Some(name)) => {
            let dir = args.get(3).ok_or_else(|| Error::from(USAGE))?;
            fs(name, dir, count(args.get(4))?)
        }
        (Some("spawn"), Some(name)) => spawn(name, count(args.get(3))?),
        _ => Err(Error::from(USAGE)),
    }
}
