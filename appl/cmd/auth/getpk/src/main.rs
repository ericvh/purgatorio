//! auth/getpk [-x] file ...: print the public key in each file, as a key
//! list names it. A file may be a seed (a user or signer key: 32 bytes for
//! Ed25519, 64 for a hybrid Ed25519 + ML-DSA-65 key), a public key file
//! (`key.pub`, named with a `.pub` suffix: 32 bytes, or 1984 for a hybrid
//! key), an authinfo or a certificate. An Ed25519 key prints as 64 hex
//! digits; a hybrid key as its id, `mldsa65:<64 hex digits>`, what trust
//! lists hold (`docs/auth.md`, "Post-quantum signatures"). `-x` prints a
//! hybrid key whole, `mldsa65:<hex of 1984 bytes>`, as a `pgcert2` holds
//! it. A `crypt -g` key file gives its public key line (`pgkempub1 <hex>`).
//!
//! `-x` and the hybrid forms are additions.

use authlib::fmt::cert::AnyCert;
use authlib::fmt::keys::AuthInfo;
use authlib::fmt::sig::{Public, Secret};
use authlib::fmt::{self};
use purgatorio::{Context, Error};

/// The public key in `path`, as printed.
fn public(path: &str, whole: bool) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    if bytes.starts_with(fmt::hybrid::SECRET_WORD.as_bytes()) {
        let k = fmt::hybrid::Secret::parse(&bytes).map_err(|e| Error(format!("{path}: {e}")))?;
        return Ok(fmt::hybrid::encode_public(&k.public()));
    }
    let key = key(path, &bytes)?;
    Ok(if whole {
        key.encode()
    } else {
        key.id().encode()
    })
}

fn key(path: &str, bytes: &[u8]) -> Result<Public, Error> {
    if let Ok(info) = AuthInfo::parse(bytes) {
        return Ok(info.key.public());
    }
    if let Ok(c) = AnyCert::parse(bytes) {
        return Ok(c.key());
    }
    let bad = || Error(format!("{path}: not a key"));
    if path.ends_with(".pub") {
        Public::from_slice(bytes).ok_or_else(bad)
    } else {
        Secret::from_seed_file(bytes)
            .map(|s| s.public())
            .ok_or_else(bad)
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let whole = args.get(1).is_some_and(|a| a == "-x");
    let files = &args[1 + usize::from(whole)..];
    if files.is_empty() {
        return Err(Error::from("usage: auth/getpk [-x] file ..."));
    }
    // As Inferno's getpk: a file it cannot read is reported and skipped,
    // with no status.
    for path in files {
        match public(path, whole) {
            Ok(k) if files.len() > 1 => println!("{k} {path}"),
            Ok(k) => println!("{k}"),
            Err(e) => {
                eprintln!("auth/getpk: {e}");
            }
        }
    }
    Ok(())
}
