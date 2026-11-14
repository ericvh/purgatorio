//! auth/getpk file ...: print the public key in each file, in hex. A file
//! may be a 32-byte seed (a user or signer key), a 32-byte public key file
//! (`key.pub`, named with a `.pub` suffix), an authinfo or a certificate.
//! A `crypt -g` key file gives its public key line (`pgkempub1 <hex>`).

use authlib::fmt::{self, cert::Cert, hex, keys::AuthInfo};
use purgatorio::{Context, Error};

/// The public key in `path`, as printed.
fn public(path: &str) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    if bytes.starts_with(fmt::hybrid::SECRET_WORD.as_bytes()) {
        let k = fmt::hybrid::Secret::parse(&bytes).map_err(|e| Error(format!("{path}: {e}")))?;
        return Ok(fmt::hybrid::encode_public(&k.public()));
    }
    ed25519(path, &bytes).map(|k| hex::encode(&k))
}

fn ed25519(path: &str, bytes: &[u8]) -> Result<[u8; fmt::PUBLIC], Error> {
    if let Ok(info) = AuthInfo::parse(bytes) {
        return Ok(fmt::public_key(&info.seed));
    }
    if let Ok(c) = Cert::parse(bytes) {
        return Ok(c.key);
    }
    let key: [u8; 32] = bytes
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
    // As Inferno's getpk: a file it cannot read is reported and skipped,
    // with no status.
    for path in &args[1..] {
        match public(path) {
            Ok(k) if args.len() > 2 => println!("{k} {path}"),
            Ok(k) => println!("{k}"),
            Err(e) => {
                eprintln!("auth/getpk: {e}");
            }
        }
    }
    Ok(())
}
