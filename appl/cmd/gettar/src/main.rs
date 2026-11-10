//! gettar [-kvR] [name ...]: unpack the tar archive on standard input into
//! the current directory. Absolute names, and names starting with `#`, are
//! made relative unless `-R` is given; `-k` keeps existing files; `-v`
//! prints each name on standard error. Names select what to extract.

use std::io;

use archive::fs::{Extract, selected};
use archive::ustar::Reader;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "gettar [-kvR] [name ...]");
    let mut x = Extract::new("gettar");
    while let Some(c) = arg.opt() {
        match c {
            'k' => x.keep = true,
            'v' => x.verbose = true,
            'R' => x.rooted = true,
            _ => return Err(arg.usage()),
        }
    }
    let names = arg.argv();
    let mut r = Reader::new(io::BufReader::new(io::stdin().lock()));
    let res = (|| {
        while let Some(h) = r.next_entry()? {
            if selected(&h.name, &names) {
                x.entry(&mut r, &h)?;
            }
        }
        Ok::<(), String>(())
    })();
    x.finish();
    res?;
    if x.failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
