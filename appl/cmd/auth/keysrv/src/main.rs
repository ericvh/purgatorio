//! auth/keysrv [-f signerkey] [-m mntpt]: the password-change server, for
//! one conversation on fd 0 (run it under `listen`). The client proves it
//! knows the old password with `pglogin1` and sends a new verifier, which
//! replaces the user's `secret` in keyfs (default `/mnt/keys`).

use std::io::Write;

use authlib::fmt::login::Verifier;
use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/keysrv [-f signerkey] [-m mntpt]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signerkey = String::from(authlib::SIGNERKEY);
    let mut mtpt = String::from(authlib::KEYS);
    while let Some(c) = arg.opt() {
        match c {
            'f' => signerkey = arg.earg()?,
            'm' => mtpt = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let fake = authlib::fake_secret(&signerkey);
    authlib::serve_login(&["passwd"], &mtpt, &fake, |hello, _, req| {
        let v = Verifier::parse(req).map_err(String::from)?;
        let path = format!("{mtpt}/{}/secret", hello.name);
        let mut f = purgatorio::open(&path, mode::OWRITE)
            .map(std::fs::File::from)
            .map_err(|e| e.0)?;
        f.write_all(v.encode().as_bytes())
            .map_err(|e| Error::from(e).0)?;
        Ok(b"ok".to_vec())
    })
}
