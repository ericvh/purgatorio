//! export [-A] [-e] [-r] [-k keyfile] [dir]: serve `dir` (default `/`) as
//! 9P on file descriptor 0. A client must prove a key or a certificate this
//! namespace trusts (`/lib/keys/users`, `/usr/<user>/key.pub`,
//! `/lib/keys/signers`; `docs/auth.md`); when it trusts nothing, nobody
//! gets in (`server trusts no keys`). `-A` asks for no proof and lets
//! everyone in: an export to a prog on the same system, over a pipe, says
//! so. `-e` (an addition) serves only encrypted conversations: a version
//! 1 or 2 proof, or an attach without a proof, fails with `encryption
//! required`. `-r` serves the tree read-only (an addition; Plan 9's
//! `exportfs -R`). A client that asks gets this server's proof, made with
//! the authinfo `keyfile` (an addition), or else this user's default key.

use purgatorio::abi::{export, mount};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "export [-A] [-e] [-r] [-k keyfile] [dir]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut noauth, mut encrypt, mut rdonly) = (false, false, false);
    let mut keyfile = String::new();
    while let Some(c) = arg.opt() {
        match c {
            'A' => noauth = true,
            'e' => encrypt = true,
            'r' => rdonly = true,
            'k' => keyfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if (noauth && encrypt) || rest.len() > 1 {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let dir = rest.first().map(String::as_str).unwrap_or("/");
    let mut flags = if noauth { mount::MNOAUTH } else { 0 };
    if rdonly {
        flags |= export::RDONLY;
    }
    if encrypt {
        flags |= export::ENCRYPT;
    }
    let keyfile = if keyfile.is_empty() {
        keyfile
    } else {
        purgatorio::auth::keyfile_path(&keyfile, &purgatorio::auth::user())
    };
    purgatorio::export_key(0, dir, flags, &keyfile)
}
