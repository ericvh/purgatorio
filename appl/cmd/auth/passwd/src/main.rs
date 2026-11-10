//! auth/passwd [-s server] [-u user]: change `user`'s password on the
//! signer through `auth/keysrv` at `server` (a dial string; default
//! `$signer`). Reads three lines on standard input: the old password, the
//! new one and the new one again. The new verifier is made here, with the
//! server's cost and a fresh salt, so neither password crosses the network.

use authlib::Rng;
use authlib::fmt::login::{Kdf, Verifier};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/passwd [-s server] [-u user]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut server = std::fs::read_to_string("/env/signer").ok();
    let mut user = authlib::user();
    while let Some(c) = arg.opt() {
        match c {
            's' => server = Some(arg.earg()?),
            'u' => user = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let server = server
        .map(|s| String::from(s.trim()))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::from("no server: use -s or set $signer"))?;
    let old = authlib::password("old password")?;
    let new = authlib::password("new password")?;
    let again = authlib::password("new password")?;
    if new != again {
        return Err(Error::from("passwords do not match"));
    }
    if new.is_empty() {
        return Err(Error::from("empty password"));
    }
    let addr = purgatorio::dial::netmkaddr(&server);
    authlib::login(&addr, "passwd", &user, &old, |kdf| {
        let fresh = Kdf::new(kdf.m, kdf.t, &mut Rng).map_err(String::from)?;
        let v = Verifier::new(new.as_bytes(), fresh).map_err(String::from)?;
        Ok(v.encode().into_bytes())
    })?;
    Ok(())
}
