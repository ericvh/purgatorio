//! auth/createsignerkey [-f keyfile] [name]: make a signer's Ed25519 key in
//! `keyfile` (default `/keydb/signerkey`) and print its public key as a
//! key-list line, ready to append to `/lib/keys/signers` (`docs/auth.md`).

use authlib::{Rng, fmt};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "auth/createsignerkey [-f keyfile] [name]");
    let mut keyfile = String::from(authlib::SIGNERKEY);
    while let Some(c) = arg.opt() {
        match c {
            'f' => keyfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() > 1 {
        return Err(Error::from(
            "usage: auth/createsignerkey [-f keyfile] [name]",
        ));
    }
    let name = rest.first().map(String::as_str).unwrap_or("");
    if name.contains(['\n', '\r']) {
        return Err(Error::from("bad name"));
    }
    let seed = Rng::seed();
    authlib::mkdirs_for(&keyfile)?;
    purgatorio::auth::write_secret(&keyfile, &seed)?;
    print!("{}", fmt::keys::list_line(&fmt::public_key(&seed), name));
    Ok(())
}
