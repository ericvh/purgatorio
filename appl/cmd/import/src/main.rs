//! import [-A] addr mtpt: dial `addr` and mount the 9P server there.
//! The mount authenticates with `fauth` when the server asks; `-A` skips
//! authentication.

use std::os::fd::AsRawFd;

use purgatorio::abi::mount::{MNOAUTH, MREPL};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "import [-A] addr mtpt");
    let mut flags = MREPL;
    while let Some(c) = arg.opt() {
        if c != 'A' {
            return Err(arg.usage());
        }
        flags |= MNOAUTH;
    }
    let rest = arg.argv();
    if rest.len() != 2 {
        return Err(Error::from("usage: import [-A] addr mtpt"));
    }
    let conn = dial::dial(&rest[0]).map_err(|e| Error(format!("import: {e}")))?;
    let fd = conn
        .data
        .as_ref()
        .ok_or_else(|| Error::from("import: no connection"))?
        .as_raw_fd();
    purgatorio::auth::amount(fd, &rest[1], flags, "").map_err(|e| Error(format!("import: {e}")))?;
    drop(conn);
    Ok(())
}
