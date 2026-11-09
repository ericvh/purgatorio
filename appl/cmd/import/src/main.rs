//! import [-A] addr mtpt: dial `addr` and mount the 9P server there.

use std::os::fd::AsRawFd;

use purgatorio::abi::mount::MREPL;
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "import [-A] addr mtpt");
    while let Some(c) = arg.opt() {
        if c != 'A' {
            return Err(arg.usage());
        }
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
    purgatorio::mount(fd, -1, &rest[1], MREPL, "").map_err(|e| Error(format!("import: {e}")))?;
    drop(conn);
    Ok(())
}
