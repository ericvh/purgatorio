//! gunzip [file ...]: decompress each `file.gz` into `file`, which replaces
//! it (`file.tgz` becomes `file.tar`), or standard input to standard
//! output.

use std::io::{self, Read, Write};

use archive::fs::warn;
use archive::gz;
use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, Dir, Error};

fn gunzip_file(path: &str) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("{path}: {e}");
    let out = if let Some(base) = path.strip_suffix(".gz") {
        String::from(base)
    } else if let Some(base) = path.strip_suffix(".tgz") {
        format!("{base}.tar")
    } else {
        return Err(err(&"no .gz suffix"));
    };
    if out.is_empty() || out.ends_with('/') {
        return Err(err(&"no .gz suffix"));
    }
    let d = purgatorio::stat(path).map_err(|e| err(&e))?;
    let z = std::fs::read(path).map_err(|_| err(&purgatorio::errstr()))?;
    let data = gz::decompress(&z).map_err(|e| err(&e))?;
    let fd = purgatorio::create(&out, mode::OWRITE, d.mode & 0o777)
        .map_err(|e| format!("{out}: {e}"))?;
    std::fs::File::from(fd)
        .write_all(&data)
        .map_err(|_| format!("{out}: {}", purgatorio::errstr()))?;
    let mut nd = Dir::null();
    nd.mtime = d.mtime;
    let _ = purgatorio::wstat(&out, &nd);
    purgatorio::remove(path).map_err(|e| err(&e))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "gunzip [file ...]");
    if arg.opt().is_some() {
        return Err(arg.usage());
    }
    let files = arg.argv();
    if files.is_empty() {
        let mut z = Vec::new();
        io::stdin().lock().read_to_end(&mut z)?;
        let data = gz::decompress(&z).map_err(Error::from)?;
        let mut out = io::stdout().lock();
        out.write_all(&data)?;
        out.flush()?;
        return Ok(());
    }
    let mut failed = false;
    for f in &files {
        if let Err(e) = gunzip_file(f) {
            warn("gunzip", &e);
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
