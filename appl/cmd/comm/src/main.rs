//! comm [-123] file1 file2: three columns from two sorted files: lines only
//! in file1, lines only in file2, and lines in both. Each digit suppresses
//! its column; `-` names the standard input.

use std::io::{self, BufRead, BufReader, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

fn open(name: &str) -> Result<Box<dyn BufRead>, Error> {
    if name == "-" {
        return Ok(Box::new(BufReader::new(io::stdin())));
    }
    match std::fs::File::open(name) {
        Ok(f) => Ok(Box::new(BufReader::new(f))),
        Err(_) => Err(Error(format!(
            "cannot open {name}: {}",
            purgatorio::errstr()
        ))),
    }
}

fn next(r: &mut dyn BufRead) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    if r.read_until(b'\n', &mut buf)? == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "comm [-123] file1 file2");
    let mut show = [true; 3];
    while let Some(o) = arg.opt() {
        match o {
            '1' => show[0] = false,
            '2' => show[1] = false,
            '3' => show[2] = false,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let files = arg.argv();
    let [f1, f2] = files.as_slice() else {
        return Err(usage);
    };
    let mut a = open(f1)?;
    let mut b = open(f2)?;
    // Column n is indented by one tab for each shown column before it.
    let prefix: Vec<String> = (0..3)
        .map(|c| "\t".repeat(show[..c].iter().filter(|s| **s).count()))
        .collect();
    let mut out = io::BufWriter::new(io::stdout().lock());
    let put = |col: usize, line: &str, out: &mut dyn Write| -> io::Result<()> {
        if show[col] {
            writeln!(out, "{}{line}", prefix[col])?;
        }
        Ok(())
    };
    let mut la = next(&mut a)?;
    let mut lb = next(&mut b)?;
    loop {
        match (&la, &lb) {
            (None, None) => break,
            (Some(x), None) => {
                put(0, x, &mut out)?;
                la = next(&mut a)?;
            }
            (None, Some(y)) => {
                put(1, y, &mut out)?;
                lb = next(&mut b)?;
            }
            (Some(x), Some(y)) => match x.cmp(y) {
                std::cmp::Ordering::Less => {
                    put(0, x, &mut out)?;
                    la = next(&mut a)?;
                }
                std::cmp::Ordering::Greater => {
                    put(1, y, &mut out)?;
                    lb = next(&mut b)?;
                }
                std::cmp::Ordering::Equal => {
                    put(2, x, &mut out)?;
                    la = next(&mut a)?;
                    lb = next(&mut b)?;
                }
            },
        }
    }
    out.flush()?;
    Ok(())
}
