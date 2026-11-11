//! grep [-cilLnsv] pattern [file...]: print lines that match a regular
//! expression, in the syntax of regexp(6).
//!
//! As in grep(1), the exit status is `fail:no matches` when no line of any
//! file matched, `fail:bad regex` for a pattern that does not compile and
//! `fail:usage` for bad arguments. A file that cannot be opened is reported
//! and skipped. `-s` prints nothing and gives only the status, `-l`
//! prints the names of files with a match and `-L` the names of files
//! without one; the status still depends on matching lines. `-c`, a count
//! of the matching lines, is Purgatorio's addition.

use std::io::{BufRead, Write};

use purgatorio::{Context, Error};
use regexp::Regex;

const USAGE: &str = "usage: grep [-cilLnsv] pattern [file...]";

#[derive(Default)]
struct Opts {
    ignore: bool,
    invert: bool,
    number: bool,
    count: bool,
    status_only: bool,
    list: bool,
    list_none: bool,
    multi: bool,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut opts = Opts::default();
    let mut i = 1;
    while i < args.len() && args[i].starts_with('-') && args[i] != "-" {
        if args[i] == "--" {
            i += 1;
            break;
        }
        for c in args[i].chars().skip(1) {
            match c {
                'n' => opts.number = true,
                'v' => opts.invert = true,
                'i' => opts.ignore = true,
                'c' => opts.count = true,
                's' => opts.status_only = true,
                'l' => opts.list = true,
                'L' => opts.list_none = true,
                _ => usage(),
            }
        }
        i += 1;
    }
    let Some(pat) = args.get(i) else { usage() };
    let pat = if opts.ignore {
        pat.to_lowercase()
    } else {
        pat.clone()
    };
    let re = match Regex::new(&pat) {
        Ok(re) => re,
        Err(e) => {
            eprintln!("grep: {e}");
            purgatorio::exits("fail:bad regex");
        }
    };
    let files: Vec<String> = args.iter().skip(i + 1).cloned().collect();
    opts.multi = files.len() > 1;
    let mut matched = 0u64;
    if files.is_empty() {
        matched += scan("stdin", &re, &opts, &mut std::io::stdin().lock())?;
    } else {
        for f in &files {
            match std::fs::File::open(f) {
                Ok(file) => {
                    matched += scan(f, &re, &opts, &mut std::io::BufReader::new(file))?;
                }
                Err(_) => {
                    eprintln!("grep: cannot open {f}: {}", Error::last());
                }
            }
        }
    }
    if matched == 0 {
        purgatorio::exits("fail:no matches");
    }
    Ok(())
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    purgatorio::exits("fail:usage");
}

/// Scans one file and returns how many lines matched. `-s` and `-l` stop
/// at the first match.
fn scan(name: &str, re: &Regex, opts: &Opts, input: &mut impl BufRead) -> Result<u64, Error> {
    let mut out = std::io::stdout().lock();
    let quiet = opts.status_only || opts.list || opts.list_none;
    let mut n = 0u64;
    let mut hits = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        n += 1;
        let body = line.strip_suffix('\n').unwrap_or(&line);
        let hit = if opts.ignore {
            re.is_match(&body.to_lowercase())
        } else {
            re.is_match(body)
        };
        if hit == opts.invert {
            continue;
        }
        hits += 1;
        if opts.status_only || opts.list {
            break;
        }
        if quiet || opts.count {
            continue;
        }
        match (opts.multi, opts.number) {
            (false, false) => {}
            (false, true) => write!(out, "{n}:")?,
            (true, false) => write!(out, "{name}: ")?,
            (true, true) => write!(out, "{name}:{n}: ")?,
        }
        out.write_all(line.as_bytes())?;
    }
    if opts.status_only {
        return Ok(hits);
    }
    if opts.list && hits > 0 || opts.list_none && hits == 0 {
        writeln!(out, "{name}")?;
    } else if opts.count && !quiet {
        if opts.multi {
            writeln!(out, "{name}:{hits}")?;
        } else {
            writeln!(out, "{hits}")?;
        }
    }
    Ok(hits)
}
