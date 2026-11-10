//! uniq [-udc] [file]: copy lines, dropping the second and later copies of
//! adjacent repeated lines. `-u` prints only lines that are not repeated,
//! `-d` one copy of each repeated line, and `-c` prefixes each output line
//! with its count and a tab.

use std::io::{self, BufRead, BufReader, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "uniq [-udc] [file]");
    let (mut u, mut d, mut c) = (false, false, false);
    while let Some(o) = arg.opt() {
        match o {
            'u' => u = true,
            'd' => d = true,
            'c' => c = true,
            _ => return Err(arg.usage()),
        }
    }
    if !u && !d {
        u = true;
        d = true;
    }
    let files = arg.argv();
    let mut input: Box<dyn BufRead> = match files.as_slice() {
        [] => Box::new(io::stdin().lock()),
        [f] => match std::fs::File::open(f) {
            Ok(file) => Box::new(BufReader::new(file)),
            Err(_) => {
                return Err(Error(format!("cannot open {f}: {}", purgatorio::errstr())));
            }
        },
        _ => return Err(arg_usage()),
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let put = |line: &[u8], n: usize, out: &mut dyn Write| -> io::Result<()> {
        if (u && n == 1) || (d && n > 1) {
            if c {
                write!(out, "{n}\t")?;
            }
            out.write_all(line)?;
        }
        Ok(())
    };
    let mut prev: Vec<u8> = Vec::new();
    let mut n = 0usize;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        if n > 0 && line == prev {
            n += 1;
        } else {
            if n > 0 {
                put(&prev, n, &mut out)?;
            }
            std::mem::swap(&mut prev, &mut line);
            n = 1;
        }
    }
    if n > 0 {
        put(&prev, n, &mut out)?;
    }
    out.flush()?;
    Ok(())
}

fn arg_usage() -> Error {
    Error::from("usage: uniq [-udc] [file]")
}
