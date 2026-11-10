//! import [-A] [-k keyfile] addr mtpt: dial `addr` and mount the 9P server
//! there. The mount authenticates with `fauth` when the server asks, with
//! the authinfo `keyfile` if given; `-A` skips authentication.

use std::os::fd::AsRawFd;

use purgatorio::abi::mount::{MNOAUTH, MREPL};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "import [-A] [-k keyfile] addr mtpt");
    let mut flags = MREPL;
    let mut keyfile = None;
    while let Some(c) = arg.opt() {
        match c {
            'A' => flags |= MNOAUTH,
            'k' => keyfile = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() != 2 {
        return Err(Error::from("usage: import [-A] [-k keyfile] addr mtpt"));
    }
    let conn = dial::dial(&rest[0]).map_err(|e| Error(format!("import: {e}")))?;
    let fd = conn
        .data
        .as_ref()
        .ok_or_else(|| Error::from("import: no connection"))?
        .as_raw_fd();
    purgatorio::auth::amount_key(fd, &rest[1], flags, "", keyfile.as_deref())
        .map_err(|e| Error(format!("import: {e}")))?;
    drop(conn);
    Ok(())
}
