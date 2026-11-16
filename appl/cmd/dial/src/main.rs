//! dial addr: connect, write standard input, then copy the reply to
//! standard output.

use purgatorio::fd::{FromRawFd, IntoRawFd};
use std::io::{Read, Write};

use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let Some(addr) = args.get(1) else {
        return Err(Error::from("usage: dial addr"));
    };
    let conn = dial::dial(addr)?;
    let data = conn.data.ok_or_else(|| Error::from("no connection"))?;
    // SAFETY: `data` owns the descriptor and this takes that ownership.
    let mut file = unsafe { purgatorio::fs::File::from_raw_fd(data.into_raw_fd()) };
    let mut request = Vec::new();
    purgatorio::io::stdin().read_to_end(&mut request)?;
    file.write_all(&request)?;
    let mut reply = Vec::new();
    file.read_to_end(&mut reply)?;
    purgatorio::io::stdout().write_all(&reply)?;
    Ok(())
}
