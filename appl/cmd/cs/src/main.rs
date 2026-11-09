//! cs addr: print the `/net/cs` translation of `addr`.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use purgatorio::dial::netmkaddr;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let Some(addr) = args.get(1) else {
        return Err(Error::from("usage: cs addr"));
    };
    let mut cs = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/net/cs")
        .map_err(|e| Error(format!("cs: {e}")))?;
    cs.write_all(netmkaddr(addr).as_bytes())
        .map_err(|e| Error(format!("cs: {e}")))?;
    cs.seek(SeekFrom::Start(0))
        .map_err(|e| Error(format!("cs: {e}")))?;
    let mut buf = String::new();
    cs.read_to_string(&mut buf)
        .map_err(|e| Error(format!("cs: {e}")))?;
    print!("{buf}");
    if !buf.ends_with('\n') {
        println!();
    }
    Ok(())
}
