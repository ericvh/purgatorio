//! auth/mkauthinfo [-f signerkey] [-e expires] user keyfile: on the
//! signer's machine, make a key for `user`, certify it with the signer key
//! (default `/keydb/signerkey`) and write the authinfo to `keyfile`.
//! `expires` is seconds since 1970 or `never`; the default is a year.

use authlib::fmt::{cert::Cert, keys::AuthInfo};
use authlib::{Rng, fmt};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/mkauthinfo [-f signerkey] [-e expires] user keyfile";
const YEAR: u64 = 365 * 24 * 60 * 60;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signerkey = String::from(authlib::SIGNERKEY);
    let mut expires = authlib::now() + YEAR;
    while let Some(c) = arg.opt() {
        match c {
            'f' => signerkey = arg.earg()?,
            'e' => expires = authlib::expiry(&arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [user, keyfile] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let signer = purgatorio::auth::read_seed(&signerkey)?;
    let seed = Rng::seed();
    let mut cert = Cert::new(user, fmt::public_key(&seed), expires).map_err(Error::from)?;
    cert.sign(&signer).map_err(Error::from)?;
    let info = AuthInfo {
        seed,
        signer: fmt::public_key(&signer),
        cert,
    };
    let path = purgatorio::auth::keyfile_path(keyfile, user);
    authlib::mkdirs_for(&path)?;
    purgatorio::auth::write_secret(&path, info.encode().as_bytes())
}
