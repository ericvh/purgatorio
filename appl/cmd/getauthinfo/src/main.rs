//! getauthinfo [-a ed25519|hybrid] [-s signer] [-u user] keyname: make a
//! new key (an Ed25519 key, or with `-a hybrid` a hybrid Ed25519 +
//! ML-DSA-65 one, which only a hybrid signer certifies), get it
//! certified by the signer (a dial string; default `$signer`) after a
//! password check, and write the authinfo `keyname`: a path, or a name
//! under `/usr/<user>/keyring` (`default` is what mounts use). The password
//! is one line on standard input; it never crosses the network
//! (`docs/auth.md`).

use authlib::fmt::cert::AnyCert;
use authlib::fmt::keys::AuthInfo;
use authlib::fmt::sig::Id;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "getauthinfo [-a ed25519|hybrid] [-s signer] [-u user] keyname";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signer = std::fs::read_to_string("/env/signer").ok();
    let mut user = authlib::user();
    let mut hybrid = false;
    while let Some(c) = arg.opt() {
        match c {
            'a' => hybrid = authlib::alg(&arg.earg()?)?,
            's' => signer = Some(arg.earg()?),
            'u' => user = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [keyname] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let signer = signer
        .map(|s| String::from(s.trim()))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::from("no signer: use -s or set $signer"))?;
    let addr = purgatorio::dial::netmkaddr(&signer);
    let password = authlib::password("password")?;
    let key = authlib::new_key(hybrid);
    let public = key.public();
    let reply = authlib::login(&addr, "cert", &user, &password, |_| {
        Ok(public.encode().into_bytes())
    })?;
    let text = String::from_utf8_lossy(&reply);
    let mut lines = text.lines();
    let signer_key = lines
        .next()
        .and_then(|l| Id::parse(l.as_bytes()))
        .ok_or_else(|| Error::from("bad reply from signer"))?;
    let cert = lines
        .next()
        .and_then(|l| AnyCert::parse(l.as_bytes()).ok())
        .ok_or_else(|| Error::from("bad certificate from signer"))?;
    if cert.key() != public || cert.name() != user || !cert.signed_by(&signer_key) {
        return Err(Error::from("bad certificate from signer"));
    }
    let info = AuthInfo {
        key,
        signer: signer_key,
        cert,
    };
    let path = purgatorio::auth::keyfile_path(keyname, &user);
    authlib::mkdirs_for(&path)?;
    purgatorio::auth::write_secret(&path, info.encode().as_bytes())
}
