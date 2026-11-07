//! wc [-lwc] [file ...]: count lines, words and bytes in each file, or
//! standard input, with a total for several files. Options choose which
//! counts to print; the default is all three.

use std::fs::File;
use std::io::{self, Read};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[derive(Default, Clone, Copy)]
struct Counts {
    lines: u64,
    words: u64,
    bytes: u64,
}

fn count(r: &mut dyn Read) -> io::Result<Counts> {
    let mut c = Counts::default();
    let mut in_word = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(c);
        }
        c.bytes += n as u64;
        for &b in &buf[..n] {
            if b == b'\n' {
                c.lines += 1;
            }
            let space = b.is_ascii_whitespace();
            if !space && !in_word {
                c.words += 1;
            }
            in_word = !space;
        }
    }
}

fn line(c: Counts, which: (bool, bool, bool), name: &str) -> String {
    let mut s = String::new();
    for (on, n) in [(which.0, c.lines), (which.1, c.words), (which.2, c.bytes)] {
        if on {
            s.push_str(&format!("{n:7} "));
        }
    }
    s.push_str(name);
    String::from(s.trim_end())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "wc [-lwc] [file ...]");
    let mut which = (false, false, false);
    while let Some(o) = arg.opt() {
        match o {
            'l' => which.0 = true,
            'w' => which.1 = true,
            'c' => which.2 = true,
            _ => return Err(arg.usage()),
        }
    }
    if which == (false, false, false) {
        which = (true, true, true);
    }
    let files = arg.argv();
    if files.is_empty() {
        println!("{}", line(count(&mut io::stdin().lock())?, which, ""));
        return Ok(());
    }
    let mut total = Counts::default();
    let mut failed = false;
    for f in &files {
        match File::open(f).and_then(|mut r| count(&mut r)) {
            Ok(c) => {
                total.lines += c.lines;
                total.words += c.words;
                total.bytes += c.bytes;
                println!("{}", line(c, which, f));
            }
            Err(e) => {
                eprintln!("wc: {f}: {}", Error::from(e));
                failed = true;
            }
        }
    }
    if files.len() > 1 {
        println!("{}", line(total, which, "total"));
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
