//! dial addr: connect, write standard input, then copy the reply to
//! standard output.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, IntoRawFd};

use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let Some(addr) = args.get(1) else {
        return Err(Error::from("usage: dial addr"));
    };
    let conn = dial::dial(addr).map_err(|e| Error(format!("dial: {e}")))?;
    let data = conn
        .data
        .ok_or_else(|| Error::from("dial: no connection"))?;
    // SAFETY: `data` owns the descriptor and this takes that ownership.
    let mut file = unsafe { std::fs::File::from_raw_fd(data.into_raw_fd()) };
    let mut request = Vec::new();
    std::io::stdin().read_to_end(&mut request)?;
    file.write_all(&request)
        .map_err(|e| Error(format!("dial: {e}")))?;
    let mut reply = Vec::new();
    file.read_to_end(&mut reply)
        .map_err(|e| Error(format!("dial: {e}")))?;
    std::io::stdout().write_all(&reply)?;
    Ok(())
}
