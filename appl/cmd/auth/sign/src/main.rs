//! auth/sign [-k keyfile] [-c certfile] ... [-o out] module: sign a module
//! (`docs/signing.md`). The signature goes into the module's
//! `purgatorio.sig` section: an entry by the same key is replaced, and
//! other entries stay, so signing a signed module countersigns it.
//!
//! With `-k` the key is a seed file or an authinfo, whose certificate
//! then goes with the signature. Without it the signature comes from the
//! guest factotum, or else from `#¤`, so the private key need not be a
//! file this prog can read. Each `-c` adds a certificate to the chain: one
//! for the key that signed the certificate before it.
//!
//! The module is rewritten in place unless `-o` names another file.

use authlib::fmt::cert::Cert;
use authlib::fmt::keys::{self, AuthInfo};
use authlib::fmt::modsig::{self, Entry};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/sign [-k keyfile] [-c certfile] ... [-o out] module";

fn read(path: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut keyfile = None;
    let mut certfiles = Vec::new();
    let mut out = None;
    while let Some(c) = arg.opt() {
        match c {
            'k' => keyfile = Some(arg.earg()?),
            'c' => certfiles.push(arg.earg()?),
            'o' => out = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [module] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let wasm = read(module)?;
    let bad = |e: &str| Error(format!("{module}: {e}"));
    let info = modsig::inspect(&wasm).map_err(|_| bad(purgatorio::abi::error::EBADEXEC))?;
    let mut extra: Vec<Cert> = Vec::new();
    for path in &certfiles {
        extra.push(
            keys::find_cert(&read(path)?)
                .ok_or_else(|| Error(format!("{path}: bad certificate")))?,
        );
    }
    let entry = match &keyfile {
        Some(path) => {
            let bytes = read(path)?;
            let (seed, mut certs) = match <[u8; authlib::fmt::SEED]>::try_from(bytes.as_slice()) {
                Ok(seed) => (seed, Vec::new()),
                Err(_) => {
                    let a =
                        AuthInfo::parse(&bytes).map_err(|_| Error(format!("{path}: not a key")))?;
                    (a.seed, vec![a.cert])
                }
            };
            certs.extend(extra);
            Entry::sign(&seed, certs, &info.digest, info.abi)
        }
        None => {
            let e = purgatorio::auth::sign_module(&info.digest, info.abi)?;
            let mut certs = e.certs().to_vec();
            certs.extend(extra);
            Entry::new(*e.key(), e.signature, certs)
        }
    }
    .map_err(bad)?;
    let signed = modsig::attach(&wasm, entry).map_err(bad)?;
    let out = out.as_deref().unwrap_or(module);
    std::fs::write(out, signed).map_err(|e| Error(format!("{out}: {}", Error::from(e))))
}
