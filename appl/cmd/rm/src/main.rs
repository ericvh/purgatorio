//! rm [-fr] name ...: remove files, and empty directories. `-r` removes
//! directories with their contents; `-f` suppresses errors.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

fn remove_all(path: &str) -> Result<(), Error> {
    let at = |e: Error| Error(format!("{path}: {e}"));
    let d = purgatorio::stat(path).map_err(at)?;
    if d.qid.is_dir() {
        let entries: Vec<String> = std::fs::read_dir(path)
            .map_err(|e| at(e.into()))?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()
            .map_err(|e| at(e.into()))?;
        for e in entries {
            remove_all(&format!("{}/{e}", path.trim_end_matches('/')))?;
        }
    }
    purgatorio::remove(path).map_err(at)
}

/// Inferno's rm prints `Usage:` and raises `fail: usage`, with a space.
fn usage() -> ! {
    eprintln!("Usage: rm [-fr] file ...");
    purgatorio::exits("fail: usage");
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "rm [-fr] name ...");
    let (mut force, mut recursive) = (false, false);
    while let Some(c) = arg.opt() {
        match c {
            'f' => force = true,
            'r' => recursive = true,
            _ => usage(),
        }
    }
    let mut failed = false;
    for name in arg.argv() {
        let res = if recursive {
            remove_all(&name)
        } else {
            purgatorio::remove(&name).map_err(|e| Error(format!("{name}: {e}")))
        };
        if let Err(e) = res
            && !force
        {
            eprintln!("rm: {e}");
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
