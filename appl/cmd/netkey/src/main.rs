//! netkey: read a password, then challenges, one per line, and print the
//! response to each: the first 8 hex digits of HMAC-SHA256 keyed with
//! SHA-256 of the password. It takes the place of Inferno's SecureNet
//! calculator, which used DES.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() != 1 {
        return Err(Error::from("usage: netkey"));
    }
    let password = authlib::password("password")?;
    while let Some(challenge) = authlib::read_line() {
        let challenge = challenge.trim();
        if challenge.is_empty() {
            continue;
        }
        println!(
            "{}",
            authlib::fmt::login::netkey_response(password.as_bytes(), challenge.as_bytes())
        );
    }
    Ok(())
}
