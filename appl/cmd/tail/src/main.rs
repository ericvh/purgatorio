//! tail [-n N] [file]: print the last N lines, 10 by default.

use std::io::{BufRead, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut n = 10usize;
    let mut i = 1usize;
    if args.get(1).map(String::as_str) == Some("-n") {
        n = args
            .get(2)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::from("usage: tail [-n N] [file]"))?;
        i = 3;
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    if let Some(path) = args.get(i) {
        let Ok(f) = std::fs::File::open(path) else {
            // As Inferno's tail: the error is printed, the status is empty.
            eprintln!("tail: {path}: {}", purgatorio::errstr());
            return Ok(());
        };
        let mut input = std::io::BufReader::new(f);
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            lines.push(line.clone());
        }
    } else {
        let mut input = std::io::stdin().lock();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            lines.push(line.clone());
        }
    }
    let start = lines.len().saturating_sub(n);
    let mut out = std::io::stdout().lock();
    for line in &lines[start..] {
        out.write_all(line.as_bytes())?;
    }
    Ok(())
}
