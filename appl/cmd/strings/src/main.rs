//! strings [file ...]: print each run of 6 or more printable characters
//! (ASCII blank to tilde, and U+00A0 to U+FFFF) with its byte offset. A run
//! longer than 70 characters is broken with ` ...` and continues on a new
//! line at its own offset. With several files, each is headed by its name.

use std::io::{self, Read, Write};

use purgatorio::{Context, Error};

const MINSPAN: usize = 6;
const BUFSIZE: usize = 70;

fn printable(c: char) -> bool {
    matches!(c as u32, 0x20..=0x7e | 0xa0..=0xffff)
}

fn strings(data: &[u8], out: &mut dyn Write) -> io::Result<()> {
    let mut run = String::new();
    let mut count = 0;
    let mut start = 0usize;
    let mut i = 0;
    while i < data.len() {
        // Decode one character, or take one bad byte as unprintable.
        let (c, n) = match std::str::from_utf8(&data[i..(i + 4).min(data.len())]) {
            Ok(s) => s.chars().next().map(|c| (Some(c), c.len_utf8())).unwrap(),
            Err(e) if e.valid_up_to() > 0 => {
                let s = std::str::from_utf8(&data[i..i + e.valid_up_to()]).unwrap();
                let c = s.chars().next().unwrap();
                (Some(c), c.len_utf8())
            }
            Err(_) => (None, 1),
        };
        match c {
            Some(c) if printable(c) => {
                if count == 0 {
                    start = i;
                }
                run.push(c);
                count += 1;
                if count == BUFSIZE {
                    writeln!(out, "{start:8}: {run} ...")?;
                    run.clear();
                    count = 0;
                }
            }
            _ => {
                if count >= MINSPAN {
                    writeln!(out, "{start:8}: {run}")?;
                }
                run.clear();
                count = 0;
            }
        }
        i += n;
    }
    if count >= MINSPAN {
        writeln!(out, "{start:8}: {run}")?;
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    if args.len() < 2 {
        let mut data = Vec::new();
        io::stdin().lock().read_to_end(&mut data)?;
        strings(&data, &mut out)?;
        out.flush()?;
        return Ok(());
    }
    let mut failed = false;
    for f in &args[1..] {
        if args.len() > 2 {
            writeln!(out, "{f}:")?;
        }
        match std::fs::read(f) {
            Ok(data) => strings(&data, &mut out)?,
            Err(_) => {
                out.flush()?;
                eprintln!("strings: cannot open {f}: {}", purgatorio::errstr());
                failed = true;
            }
        }
    }
    out.flush()?;
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
