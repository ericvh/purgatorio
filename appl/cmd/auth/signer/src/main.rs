//! auth/signer [-f signerkey] [-m mntpt] [-e days]: the certificate server,
//! for one conversation on fd 0 (run it under `listen` or `styxlisten`).
//! It checks the client's password against keyfs (default `/mnt/keys`)
//! with `pglogin1`, then signs the public key the client sent with the
//! signer key (default `/keydb/signerkey`). The certificate expires with
//! the account, and at most `days` (default 365) from now.

use authlib::fmt::{self, cert::Cert, hex};
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
    let seed = purgatorio::auth::read_seed(&signerkey)?;
    let signer = fmt::public_key(&seed);
    authlib::serve_login(&["cert"], &mtpt, &seed, |hello, acct, req| {
        let key = std::str::from_utf8(req)
            .ok()
            .and_then(|s| hex::array::<{ fmt::PUBLIC }>(s.trim().as_bytes()))
            .ok_or_else(|| String::from("bad public key"))?;
        let cap = authlib::now().saturating_add(days.saturating_mul(DAY));
        let expires = if acct.expire == 0 {
            cap
        } else {
            acct.expire.min(cap)
        };
        let mut cert = Cert::new(&hello.name, key, expires).map_err(String::from)?;
        cert.sign(&seed).map_err(String::from)?;
        Ok(format!("ok\n{}\n{}\n", hex::encode(&signer), cert.encode()).into_bytes())
    })
}
