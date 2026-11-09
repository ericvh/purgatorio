//! export [-A] [dir]: serve `dir` (default `/`) as 9P on file descriptor 0.
//! `-A` does not authenticate, even when the tree holds a public key.

use purgatorio::abi::mount;
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
    let flags = if noauth { mount::MNOAUTH } else { 0 };
    purgatorio::export(0, dir, flags).map_err(|e| Error(format!("export: {e}")))
}
