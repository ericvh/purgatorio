//! xd [file]: hex dump, 16 bytes per line.

use std::io::{Read, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut data = Vec::new();
    if let Some(path) = args.get(1) {
        if std::fs::read(path).map(|d| data = d).is_err() {
            eprintln!("xd: cannot open {path}: {}", purgatorio::errstr());
            purgatorio::exits("fail:cannot open");
        }
    } else {
        std::io::stdin().read_to_end(&mut data)?;
    }
    let mut out = std::io::stdout().lock();
    for (i, chunk) in data.chunks(16).enumerate() {
        write!(out, "{:08x}  ", i * 16)?;
        for b in chunk {
            write!(out, "{b:02x} ")?;
        }
        for _ in chunk.len()..16 {
            write!(out, "   ")?;
        }
        write!(out, " ")?;
        for &b in chunk {
            let c = if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            };
            write!(out, "{c}")?;
        }
        writeln!(out)?;
    }
    Ok(())
}
