//! freq [-dxocr] [file ...]: print how often each byte value occurs, or
//! with `-r` each UTF-8 character. Each line shows the value in decimal,
//! octal, hex and as a character (`-` when unprintable), then the count;
//! `-d`, `-o`, `-x` and `-c` choose a subset of the formats.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (mut dec, mut oct, mut hex, mut chr, mut rune) = (false, false, false, false, false);
    let mut i = 1;
    while i < args.len() && args[i].starts_with('-') && args[i] != "-" {
        for c in args[i].chars().skip(1) {
            match c {
                'd' => dec = true,
                'o' => oct = true,
                'x' => hex = true,
                'c' => chr = true,
                'r' => rune = true,
                _ => {
                    eprintln!("freq: unknown option {c}");
                    purgatorio::exits("fail:usage");
                }
            }
        }
        i += 1;
    }
    if !(dec || oct || hex || chr) {
        (dec, oct, hex, chr) = (true, true, true, true);
    }
    let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
    let mut add = |data: &[u8]| {
        if rune {
            for c in String::from_utf8_lossy(data).chars() {
                *counts.entry(c as u32).or_default() += 1;
            }
        } else {
            for &b in data {
                *counts.entry(u32::from(b)).or_default() += 1;
            }
        }
    };
    let mut failed = false;
    if i >= args.len() {
        let mut data = Vec::new();
        io::stdin().lock().read_to_end(&mut data)?;
        add(&data);
    }
    for f in &args[i..] {
        match std::fs::read(f) {
            Ok(data) => add(&data),
            Err(_) => {
                eprintln!("freq: cannot open {f}: {}", purgatorio::errstr());
                failed = true;
            }
        }
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    for (&v, &n) in &counts {
        if dec {
            write!(out, "{v:3} ")?;
        }
        if oct {
            write!(out, "{v:03o} ")?;
        }
        if hex {
            write!(out, "{v:02x} ")?;
        }
        if chr {
            let unprintable = v <= 0x20 || (0x7f..0xa0).contains(&v) || (v > 0xff && !rune);
            match char::from_u32(v) {
                Some(c) if !unprintable => write!(out, "{c} ")?,
                _ => write!(out, "- ")?,
            }
        }
        writeln!(out, "{n:8}")?;
    }
    out.flush()?;
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
