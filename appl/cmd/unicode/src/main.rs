//! unicode [-nt] hexmin-hexmax | hex ... | char ...: convert between
//! characters and their Unicode values. A range prints a table of values
//! and characters, eight to a line. Arguments that start with a hex digit
//! are values, printed as characters one per line (`-t` puts them on one
//! line); other arguments are text whose characters are printed as hex
//! values. `-n` forces the value output.

use std::io::{self, Write};

use purgatorio::{Context, Error};

const USAGE: &str = "usage: unicode { [-t] hex hex ... | hexmin-hexmax ... | [-n] char ... }";

fn hex_start(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_hexdigit())
}

fn value(s: &str) -> Option<char> {
    u32::from_str_radix(s, 16).ok().and_then(char::from_u32)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // Inferno's unicode prints its usage and exits with no status.
    run(args).map_err(|e| e.usage_exits("usage: ", ""))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let (mut numout, mut text) = (false, false);
    let mut i = 1;
    while let Some(a) = args.get(i).filter(|a| a.starts_with('-')) {
        match a.as_str() {
            "-n" => numout = true,
            "-t" => text = true,
            _ => {}
        }
        i += 1;
    }
    let words = &args[i.min(args.len())..];
    if words.is_empty() {
        return Err(Error::from(USAGE));
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    if !numout && words[0].contains('-') {
        for q in words {
            let range = q
                .split_once('-')
                .filter(|(a, _)| hex_start(a))
                .and_then(|(a, b)| Some((value(a)?, value(b)?)))
                .filter(|(a, b)| a <= b);
            let Some((lo, hi)) = range else {
                out.flush()?;
                return Err(Error(format!("bad range {q}")));
            };
            let cs: Vec<char> = (lo..=hi).collect();
            for (k, c) in cs.iter().enumerate() {
                write!(out, "{:04x} {c}", *c as u32)?;
                let sep = if k + 1 == cs.len() || (k + 1) % 8 == 0 {
                    '\n'
                } else {
                    '\t'
                };
                write!(out, "{sep}")?;
            }
        }
    } else if numout || !hex_start(&words[0]) {
        for q in words {
            for c in q.chars() {
                writeln!(out, "{:04x}", c as u32)?;
            }
        }
    } else {
        for q in words {
            let Some(c) = value(q) else {
                out.flush()?;
                return Err(Error(format!("bad unicode value {q}")));
            };
            write!(out, "{c}")?;
            if !text {
                writeln!(out)?;
            }
        }
        if text {
            writeln!(out)?;
        }
    }
    out.flush()?;
    Ok(())
}
