//! auth/mkauthinfo [-a ed25519|hybrid] [-f signerkey] [-e expires] user
//! keyfile: on the signer's machine, make a key for `user`, certify it
//! with the signer key (default `/keydb/signerkey`) and write the authinfo
//! to `keyfile`. `expires` is seconds since 1970 or `never`; the default
//! is a year. The key is of the signer's kind: Ed25519 with a `pgcert1`,
//! or with a hybrid signer key (`createsignerkey -a hybrid`) a hybrid
//! Ed25519 + ML-DSA-65 key with a `pgcert2`. `-a` names the kind, and one
//! the signer key is not is refused: a signer certifies only keys of its
//! own kind.

use authlib::fmt::cert::{AnyCert, Cert, Cert2};
use authlib::fmt::keys::AuthInfo;
use authlib::fmt::sig::Secret;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/mkauthinfo [-a ed25519|hybrid] [-f signerkey] [-e expires] user keyfile";
const YEAR: u64 = 365 * 24 * 60 * 60;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signerkey = String::from(authlib::SIGNERKEY);
    let mut expires = authlib::now() + YEAR;
    let mut alg = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => alg = Some(authlib::alg(&arg.earg()?)?),
            'f' => signerkey = arg.earg()?,
            'e' => expires = authlib::expiry(&arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [user, keyfile] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let signer = purgatorio::auth::read_secret(&signerkey)?;
    let hybrid = signer.is_hybrid();
    if alg.is_some_and(|a| a != hybrid) {
        return Err(Error(format!(
            "{signerkey}: the signer key is {}; it certifies only {} keys",
            authlib::kind(hybrid),
            authlib::kind(hybrid)
        )));
    }
    let key = authlib::new_key(hybrid);
    let mut cert = match &key {
        Secret::Ed25519(s) => {
            AnyCert::V1(Cert::new(user, authlib::fmt::public_key(s), expires).map_err(Error::from)?)
        }
        Secret::Hybrid(h) => {
            AnyCert::V2(Cert2::new(user, h.public(), expires).map_err(Error::from)?)
        }
    };
    cert.sign(&signer).map_err(Error::from)?;
    let info = AuthInfo {
        key,
        signer: signer.public().id(),
        cert,
    };
    let path = purgatorio::auth::keyfile_path(keyfile, user);
    authlib::mkdirs_for(&path)?;
    purgatorio::auth::write_secret(&path, info.encode().as_bytes())
}
