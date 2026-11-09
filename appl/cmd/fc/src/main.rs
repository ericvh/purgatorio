//! fc src... dest: copy files. The last argument is the destination.

use std::path::Path;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 3 {
        return Err(Error::from("usage: fc src... dest"));
    }
    let dest = Path::new(args.last().unwrap());
    let srcs = &args[1..args.len() - 1];
    for src in srcs {
        let to = if dest.is_dir() {
            dest.join(Path::new(src).file_name().unwrap_or_default())
        } else if srcs.len() == 1 {
            dest.to_path_buf()
        } else {
            return Err(Error::from("dest is not a directory"));
        };
        let data = std::fs::read(src).map_err(|_| Error::last())?;
        std::fs::write(&to, data).map_err(|_| Error::last())?;
    }
    Ok(())
}
