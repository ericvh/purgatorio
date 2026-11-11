//! mkdir [-p] dir ...: make directories. With `-p`, missing parents are
//! made too and existing directories are not an error.

use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error, abi::mode};

fn make(path: &str) -> Result<(), Error> {
    if purgatorio::stat(path).is_ok() {
        return Err(Error::from(purgatorio::abi::error::EEXIST));
    }
    purgatorio::create(path, mode::OREAD, DMDIR | 0o777).map(drop)
}

fn make_all(path: &str) -> Result<(), Error> {
    match purgatorio::stat(path) {
        Ok(d) if d.qid.is_dir() => return Ok(()),
        Ok(_) => return Err(Error::from(purgatorio::abi::error::ENOTDIR)),
        Err(_) => {}
    }
    if let Some((parent, _)) = path.trim_end_matches('/').rsplit_once('/')
        && !parent.is_empty()
    {
        make_all(parent)?;
    }
    make(path)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "mkdir [-p] dir ...");
    let mut parents = false;
    while let Some(c) = arg.opt() {
        match c {
            'p' => parents = true,
            _ => return Err(arg.usage()),
        }
    }
    let dirs = arg.argv();
    if dirs.is_empty() {
        return Err(Error::from("usage: mkdir [-p] dir ..."));
    }
    let mut failed = false;
    for d in &dirs {
        let res = if parents { make_all(d) } else { make(d) };
        if let Err(e) = res {
            eprintln!("mkdir: {d}: {e}");
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:error");
    }
    Ok(())
}
