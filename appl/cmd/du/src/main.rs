//! du path...: print the byte size of each path. A name it cannot stat
//! or read is reported and skipped, with no status, as in Inferno.

use std::path::Path;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let paths: Vec<&str> = if args.len() > 1 {
        args[1..].iter().map(String::as_str).collect()
    } else {
        vec!["."]
    };
    for p in paths {
        match size(Path::new(p)) {
            Ok(n) => println!("{n}\t{p}"),
            Err((why, e)) => eprintln!("du: {why} {}: {e}", purgatorio::ns::quoted(p)),
        }
    }
    Ok(())
}

/// The size of `path`, or what failed and the error.
fn size(path: &Path) -> Result<u64, (&'static str, Error)> {
    let meta = std::fs::metadata(path).map_err(|_| ("can't stat", Error::last()))?;
    if meta.is_dir() {
        let mut n = 0;
        let read = |_| ("can't read", Error::last());
        for e in std::fs::read_dir(path).map_err(read)? {
            let e = e.map_err(read)?;
            n += size(&e.path())?;
        }
        Ok(n)
    } else {
        Ok(meta.len())
    }
}
