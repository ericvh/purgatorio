//! export [-A] [-r] [dir]: serve `dir` (default `/`) as 9P on file
//! descriptor 0. `-A` does not authenticate, even when the tree holds a
//! public key. `-r` serves the tree read-only (an addition; Plan 9's
//! `exportfs -R`).

use purgatorio::abi::{export, mount};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let noauth = args.iter().any(|a| a == "-A");
    let dir = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("/");
    let mut flags = if noauth { mount::MNOAUTH } else { 0 };
    if args.iter().any(|a| a == "-r") {
        flags |= export::RDONLY;
    }
    purgatorio::export(0, dir, flags).map_err(|e| Error(format!("export: {e}")))
}
