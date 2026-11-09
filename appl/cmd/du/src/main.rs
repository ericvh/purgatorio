//! du path...: print the byte size of each path.

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
        println!("{}\t{p}", size(Path::new(p))?);
    }
    Ok(())
}

fn size(path: &Path) -> Result<u64, Error> {
    let meta = std::fs::metadata(path).map_err(|_| Error::last())?;
    if meta.is_dir() {
        let mut n = 0;
        for e in std::fs::read_dir(path).map_err(|_| Error::last())? {
            let e = e.map_err(|_| Error::last())?;
            n += size(&e.path())?;
        }
        Ok(n)
    } else {
        Ok(meta.len())
    }
}
