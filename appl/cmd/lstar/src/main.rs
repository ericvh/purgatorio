//! lstar: list the tar archive on standard input, one entry per line as
//! `name mtime size 0`, the format of `du -n -t`.

use std::io::{self, Write};

use archive::ustar::Reader;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() > 1 {
        return Err(Error::from("usage: lstar"));
    }
    let mut r = Reader::new(io::BufReader::new(io::stdin().lock()));
    let mut out = io::BufWriter::new(io::stdout().lock());
    while let Some(h) = r.next_entry()? {
        writeln!(out, "{} {} {} 0", h.name, h.mtime, h.size)?;
    }
    out.flush()?;
    Ok(())
}
