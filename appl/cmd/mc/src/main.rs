//! mc [-c columns] [file ...]: print the input lines in as many columns as
//! fit across `columns` characters (default 65), filling each column top
//! to bottom.

use std::io::{self, BufRead, BufReader, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "mc [-c columns] [file ...]");
    let mut columns = 65usize;
    while let Some(o) = arg.opt() {
        match o {
            'c' => {
                let s = arg.earg()?;
                columns = s.parse().map_err(|_| arg.usage())?;
            }
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    let mut words: Vec<String> = Vec::new();
    let mut read = |r: &mut dyn BufRead| -> io::Result<()> {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf)? == 0 {
                return Ok(());
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            words.push(String::from_utf8_lossy(&buf).into_owned());
        }
    };
    if files.is_empty() {
        read(&mut io::stdin().lock())?;
    }
    for f in &files {
        match std::fs::File::open(f) {
            Ok(file) => read(&mut BufReader::new(file))?,
            Err(_) => {
                // As Inferno's mc: reported, skipped, no status.
                eprintln!("mc: can't open {f}: {}", purgatorio::errstr());
            }
        }
    }
    let width = |s: &str| s.chars().count();
    let maxwidth = words.iter().map(|w| width(w)).max().unwrap_or(0) + 1;
    let ncols = (columns / maxwidth).max(1);
    let nlines = words.len().div_ceil(ncols);
    let mut out = io::BufWriter::new(io::stdout().lock());
    for i in 0..nlines {
        let mut line = String::new();
        let mut j = i;
        while j < words.len() {
            line.push_str(&words[j]);
            if j + nlines < words.len() {
                let pad = maxwidth - width(&words[j]);
                line.push_str(&" ".repeat(pad));
            }
            j += nlines;
        }
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    Ok(())
}
