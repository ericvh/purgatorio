//! sort [-nr] [file]: sort lines into increasing order, stably. `-n`
//! compares an initial signed integer on each line; `-r` reverses the
//! order.

use std::cmp::Ordering;
use std::io::{self, Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

/// The value of the signed integer that starts `line`, after any blanks,
/// as a sign and a digit string without leading zeros, so integers of any
/// size compare exactly. A line with no number counts as zero.
fn number(line: &str) -> (bool, &str) {
    let s = line.trim_start_matches([' ', '\t']);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    let digits = s[..end].trim_start_matches('0');
    (neg && !digits.is_empty(), digits)
}

fn numcmp(a: &str, b: &str) -> Ordering {
    let (an, ad) = number(a);
    let (bn, bd) = number(b);
    let mag = ad.len().cmp(&bd.len()).then_with(|| ad.cmp(bd));
    match (an, bn) {
        (false, false) => mag,
        (true, true) => mag.reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "sort [-nr] [file]");
    let (mut numeric, mut reverse) = (false, false);
    while let Some(o) = arg.opt() {
        match o {
            'n' => numeric = true,
            'r' => reverse = true,
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    let mut data = Vec::new();
    match files.as_slice() {
        [] => {
            io::stdin().lock().read_to_end(&mut data)?;
        }
        [f] => {
            // As Inferno's sort: `cannot open`, and `fail:open file`.
            let Ok(d) = std::fs::read(f) else {
                eprintln!("sort: cannot open {f}: {}", purgatorio::errstr());
                purgatorio::exits("fail:open file");
            };
            data = d;
        }
        _ => return Err(Error::from("usage: sort [-nr] [file]")),
    }
    let text = String::from_utf8_lossy(&data);
    let mut lines: Vec<&str> = text
        .split_inclusive('\n')
        .map(|l| l.strip_suffix('\n').unwrap_or(l))
        .collect();
    lines.sort_by(|a, b| {
        let o = if numeric { numcmp(a, b) } else { a.cmp(b) };
        if reverse { o.reverse() } else { o }
    });
    let mut out = io::BufWriter::new(io::stdout().lock());
    for l in lines {
        out.write_all(l.as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
