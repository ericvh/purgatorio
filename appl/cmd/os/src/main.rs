//! os command...: run a host command through `#C` and copy its output here.

use std::fs::OpenOptions;
use std::io::{Read, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        return Err(Error::from("usage: os command..."));
    }
    let line = args[1..].join(" ");
    let mut clone = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/cmd/clone")?;
    let mut idbuf = [0u8; 32];
    let n = clone.read(&mut idbuf)?;
    let id = std::str::from_utf8(&idbuf[..n]).unwrap_or("").trim();
    clone.write_all(line.as_bytes())?;
    let mut data = std::fs::File::open(format!("/cmd/{id}/data"))?;
    let mut out = Vec::new();
    data.read_to_end(&mut out)?;
    std::io::stdout().write_all(&out)?;
    let err = std::fs::read(format!("/cmd/{id}/stderr")).unwrap_or_default();
    std::io::stderr().write_all(&err)?;
    let status = std::fs::read_to_string(format!("/cmd/{id}/wait")).unwrap_or_default();
    let status = status.trim();
    if status.is_empty() {
        Ok(())
    } else {
        Err(Error(String::from(status)))
    }
}
