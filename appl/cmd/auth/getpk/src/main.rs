//! auth/getpk file ...: print the public key in each file, in hex. A file
//! may be a 32-byte seed (a user or signer key), a 32-byte public key file
//! (`key.pub`, named with a `.pub` suffix), an authinfo or a certificate.

use authlib::fmt::{self, cert::Cert, hex, keys::AuthInfo};
use purgatorio::{Context, Error};

fn public(path: &str) -> Result<[u8; fmt::PUBLIC], Error> {
    let bytes = std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    if let Ok(info) = AuthInfo::parse(&bytes) {
        return Ok(fmt::public_key(&info.seed));
    }
    if let Ok(c) = Cert::parse(&bytes) {
        return Ok(c.key);
    }
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error(format!("{path}: not a key")))?;
    if path.ends_with(".pub") {
        Ok(key)
    } else {
        Ok(fmt::public_key(&key))
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        return Err(Error::from("usage: auth/getpk file ..."));
    }
    let mut failed = false;
    for path in &args[1..] {
        match public(path) {
            Ok(k) if args.len() > 2 => println!("{} {path}", hex::encode(&k)),
            Ok(k) => println!("{}", hex::encode(&k)),
            Err(e) => {
                eprintln!("auth/getpk: {e}");
                failed = true;
            }
        }
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
