//! export [-A] [-e] [-r] [dir]: serve `dir` (default `/`) as 9P on file
//! descriptor 0. `-A` does not authenticate, even when this namespace
//! trusts a key or a signer (`/lib/keys/signers`, `/usr/<user>/key.pub`;
//! `docs/auth.md`). `-e` (an addition) serves only encrypted
//! conversations: a version 1 or 2 proof, or an attach without a proof,
//! fails with `encryption required`. `-r` serves the tree read-only (an
//! addition; Plan 9's `exportfs -R`). A client that asks gets this
//! server's proof, made with this user's default key.

use purgatorio::abi::{export, mount};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let noauth = args.iter().any(|a| a == "-A");
    let encrypt = args.iter().any(|a| a == "-e");
    if noauth && encrypt {
        return Err(Error::from("usage: export [-A] [-e] [-r] [dir]"));
    }
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
    if encrypt {
        flags |= export::ENCRYPT;
    }
    purgatorio::export(0, dir, flags).map_err(|e| Error(format!("export: {e}")))
}
