//! puttar [file ...]: write a tar archive of each file, and the contents of
//! each directory, to standard output. With no arguments, read file names
//! from standard input, one per line, and add each without descending into
//! directories.

use std::io::{self, BufRead, Write};

use archive::fs::Put;
use archive::ustar::Writer;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut w = Writer::new(&mut out);
    let mut put = Put {
        prog: "puttar",
        recurse: args.len() > 1,
        verbose: false,
        failed: false,
    };
    if args.len() > 1 {
        for f in &args[1..] {
            put.add(&mut w, f)?;
        }
    } else {
        for line in io::stdin().lock().lines() {
            let line = line?;
            if !line.is_empty() {
                put.add(&mut w, &line)?;
            }
        }
    }
    w.finish()?;
    out.flush()?;
    if put.failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
