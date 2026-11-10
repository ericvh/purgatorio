//! auth/changelogin [-m mntpt] [-C m,t] [-e expires] user: on the signer's
//! machine, add `user` to keyfs (default `/mnt/keys`) or change its
//! password. The password is one line on standard input. The verifier
//! stored is Argon2id at cost `-C m,t` (default 19456 KiB, 2 passes).
//! `expires` is seconds since 1970 or `never`; a new user gets a year.

use std::io::Write;

use authlib::Rng;
use authlib::fmt::login::{DEFAULT_M, DEFAULT_T, Kdf, Verifier};
use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/changelogin [-m mntpt] [-C m,t] [-e expires] user";
const YEAR: u64 = 365 * 24 * 60 * 60;

fn put(path: &str, value: &str) -> Result<(), Error> {
    let mut f = purgatorio::open(path, mode::OWRITE)
        .map(std::fs::File::from)
        .map_err(|e| Error(format!("{path}: {}", e)))?;
    f.write_all(value.as_bytes())
        .map_err(|e| Error(format!("{path}: {}", Error::from(e))))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut mtpt = String::from(authlib::KEYS);
    let (mut m, mut t) = (DEFAULT_M, DEFAULT_T);
    let mut expires = None;
    while let Some(c) = arg.opt() {
        match c {
            'm' => mtpt = arg.earg()?,
            'C' => (m, t) = authlib::cost(&arg.earg()?)?,
            'e' => expires = Some(authlib::expiry(&arg.earg()?)?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [user] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    if !authlib::fmt::cert::valid_name(user) {
        return Err(Error(format!("bad user name {user}")));
    }
    let password = authlib::password("password")?;
    if password.is_empty() {
        return Err(Error::from("empty password"));
    }
    let dir = format!("{mtpt}/{user}");
    let new = purgatorio::stat(&dir).is_err();
    if new {
        purgatorio::create(&dir, mode::OREAD, purgatorio::DMDIR | 0o700)
            .map_err(|e| Error(format!("{dir}: {}", e)))?;
    }
    let kdf = Kdf::new(m, t, &mut Rng).map_err(Error::from)?;
    let v = Verifier::new(password.as_bytes(), kdf).map_err(Error::from)?;
    put(&format!("{dir}/secret"), &v.encode())?;
    let expires = match expires {
        Some(e) => Some(e),
        None if new => Some(authlib::now() + YEAR),
        None => None,
    };
    if let Some(e) = expires {
        put(&format!("{dir}/expire"), &authlib::show_expiry(e))?;
    }
    put(&format!("{dir}/status"), "ok")
}
