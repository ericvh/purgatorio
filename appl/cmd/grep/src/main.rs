//! grep [-inv] pattern [file...]: print lines that contain a literal pattern.

use std::io::{BufRead, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut number = false;
    let mut invert = false;
    let mut ignore = false;
    let mut count = false;
    let mut i = 1;
    while i < args.len() && args[i].starts_with('-') && args[i] != "-" {
        for c in args[i].chars().skip(1) {
            match c {
                'n' => number = true,
                'v' => invert = true,
                'i' => ignore = true,
                'c' => count = true,
                _ => return Err(Error::from("usage: grep [-inv] pattern [file...]")),
            }
        }
        i += 1;
    }
    let pat = args
        .get(i)
        .ok_or_else(|| Error::from("usage: grep [-inv] pattern [file...]"))?;
    let pat = if ignore {
        pat.to_lowercase()
    } else {
        pat.clone()
    };
    let files: Vec<String> = args.iter().skip(i + 1).cloned().collect();
    let opts = Scan {
        ignore,
        invert,
        number,
        count,
        multi: files.len() > 1,
    };
    if files.is_empty() {
        scan("-", &pat, &opts, &mut std::io::stdin().lock())?;
    } else {
        for f in &files {
            let file = std::fs::File::open(f).map_err(|_| Error::last())?;
            scan(f, &pat, &opts, &mut std::io::BufReader::new(file))?;
        }
    }
    Ok(())
}

struct Scan {
    ignore: bool,
    invert: bool,
    number: bool,
    count: bool,
    multi: bool,
}

fn scan(name: &str, pat: &str, opts: &Scan, input: &mut impl BufRead) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();
    let mut n = 0u64;
    let mut hits = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        n += 1;
        let hay = if opts.ignore {
            line.to_lowercase()
        } else {
            line.clone()
        };
        if hay.contains(pat) == !opts.invert {
            hits += 1;
            if opts.count {
                continue;
            }
            if opts.multi {
                write!(out, "{name}:")?;
            }
            if opts.number {
                write!(out, "{n}:")?;
            }
            out.write_all(line.as_bytes())?;
        }
    }
    if opts.count {
        if opts.multi {
            writeln!(out, "{name}:{hits}")?;
        } else {
            writeln!(out, "{hits}")?;
        }
    }
    Ok(())
}
