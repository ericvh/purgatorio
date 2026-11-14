//! auth/createsignerkey [-a ed25519|hybrid] [-f keyfile] [name]: make a
//! signer's key in `keyfile` (default `/keydb/signerkey`) and print its
//! public key as a key-list line, ready to append to `/lib/keys/signers`
//! (`docs/auth.md`). `-a hybrid` makes a hybrid Ed25519 + ML-DSA-65 key
//! (a 64-byte seed file), listed by its id as `mldsa65:<hex>`; the default
//! is Ed25519.

use authlib::fmt;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/createsignerkey [-a ed25519|hybrid] [-f keyfile] [name]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut keyfile = String::from(authlib::SIGNERKEY);
    let mut hybrid = false;
    while let Some(c) = arg.opt() {
        match c {
            'a' => hybrid = authlib::alg(&arg.earg()?)?,
            'f' => keyfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() > 1 {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let name = rest.first().map(String::as_str).unwrap_or("");
    if name.contains(['\n', '\r']) {
        return Err(Error::from("bad name"));
    }
    let key = authlib::new_key(hybrid);
    authlib::mkdirs_for(&keyfile)?;
    purgatorio::auth::write_secret(&keyfile, key.seed_bytes())?;
    print!("{}", fmt::keys::list_line(key.public().id(), name));
    Ok(())
}
