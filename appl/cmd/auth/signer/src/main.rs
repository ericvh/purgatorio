//! auth/signer [-f signerkey] [-m mntpt] [-e days]: the certificate server,
//! for one conversation on fd 0 (run it under `listen` or `styxlisten`).
//! It checks the client's password against keyfs (default `/mnt/keys`)
//! with `pglogin1`, then signs the public key the client sent with the
//! signer key (default `/keydb/signerkey`). The certificate expires with
//! the account, and at most `days` (default 365) from now.
//!
//! The key sent is an Ed25519 key in hex, or a hybrid Ed25519 + ML-DSA-65
//! key as `mldsa65:<hex>` (`getauthinfo -a hybrid`). The signer certifies
//! only keys of its own kind: an Ed25519 signer key makes `pgcert1`
//! certificates, a hybrid one (`createsignerkey -a hybrid`) `pgcert2`
//! ones. The reply's signer line is the signer's id as `/lib/keys/signers`
//! lists it.

use authlib::fmt::cert::{AnyCert, Cert, Cert2};
use authlib::fmt::sig::Public;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/signer [-f signerkey] [-m mntpt] [-e days]";
const DAY: u64 = 24 * 60 * 60;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signerkey = String::from(authlib::SIGNERKEY);
    let mut mtpt = String::from(authlib::KEYS);
    let mut days = 365u64;
    while let Some(c) = arg.opt() {
        match c {
            'f' => signerkey = arg.earg()?,
            'm' => mtpt = arg.earg()?,
            'e' => days = arg.earg()?.parse().map_err(|_| Error::from("bad days"))?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let secret = purgatorio::auth::read_secret(&signerkey)?;
    let signer = secret.public().id();
    authlib::serve_login(&["cert"], &mtpt, secret.seed_bytes(), |hello, acct, req| {
        let key = std::str::from_utf8(req)
            .ok()
            .and_then(|s| Public::parse(s.trim().as_bytes()))
            .ok_or_else(|| String::from("bad public key"))?;
        if key.is_hybrid() != secret.is_hybrid() {
            return Err(format!(
                "this signer certifies only {} keys",
                authlib::kind(secret.is_hybrid())
            ));
        }
        let cap = authlib::now().saturating_add(days.saturating_mul(DAY));
        let expires = if acct.expire == 0 {
            cap
        } else {
            acct.expire.min(cap)
        };
        let mut cert = match key {
            Public::Ed25519(k) => {
                AnyCert::V1(Cert::new(&hello.name, k, expires).map_err(String::from)?)
            }
            Public::Hybrid(h) => {
                AnyCert::V2(Cert2::new(&hello.name, h, expires).map_err(String::from)?)
            }
        };
        cert.sign(&secret).map_err(String::from)?;
        Ok(format!("ok\n{}\n{}\n", signer.encode(), cert.encode()).into_bytes())
    })
}
