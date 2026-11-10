//! gzip [-v] [-level] [file ...]: compress each file into `file.gz`, which
//! replaces it, or standard input to standard output. `level` is a digit
//! from 1 to 9 (default 6); `-v` reports each file's compression ratio.
//!
//! The gzip header carries no name and a zero time, so output depends only
//! on the input and the level.

use std::io::{self, Read, Write};

use archive::fs::warn;
use archive::gz;
use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, Dir, Error};

fn gzip_file(path: &str, level: u8, verbose: bool) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("{path}: {e}");
    if path.ends_with(".gz") {
        return Err(err(&"already has .gz suffix"));
    }
    let d = purgatorio::stat(path).map_err(|e| err(&e))?;
    if d.qid.is_dir() {
        return Err(err(&"is a directory"));
    }
    let data = std::fs::read(path).map_err(|_| err(&purgatorio::errstr()))?;
    let z = gz::compress(&data, level);
    let out = format!("{path}.gz");
    let fd = purgatorio::create(&out, mode::OWRITE, d.mode & 0o777)
        .map_err(|e| format!("{out}: {e}"))?;
    std::fs::File::from(fd)
        .write_all(&z)
        .map_err(|_| format!("{out}: {}", purgatorio::errstr()))?;
    let mut nd = Dir::null();
    nd.mtime = d.mtime;
    let _ = purgatorio::wstat(&out, &nd);
    purgatorio::remove(path).map_err(|e| err(&e))?;
    if verbose {
        let ratio = if data.is_empty() {
            0.0
        } else {
            100.0 * (1.0 - z.len() as f64 / data.len() as f64)
        };
        warn("gzip", &format!("{path}: {ratio:.1}%"));
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "gzip [-v] [-1-9] [file ...]");
    let mut level = gz::DEFAULT_LEVEL;
    let mut verbose = false;
    while let Some(c) = arg.opt() {
        match c {
            'v' => verbose = true,
            '1'..='9' => level = c as u8 - b'0',
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    if files.is_empty() {
        let mut data = Vec::new();
        io::stdin().lock().read_to_end(&mut data)?;
        let mut out = io::stdout().lock();
        out.write_all(&gz::compress(&data, level))?;
        out.flush()?;
        return Ok(());
    }
    let mut failed = false;
    for f in &files {
        if let Err(e) = gzip_file(f, level, verbose) {
            warn("gzip", &e);
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
