//! auth/verify [-s signers] [-r revoked] [-u name] [-t time] file: check the
//! certificate in `file` (a certificate or an authinfo) as a server would,
//! against a signer list (default `/lib/keys/signers`) and a revocation
//! list (default `/lib/keys/revoked`) at `time` (default now). Prints the
//! certificate's name, key, expiry and each signer with its standing, and
//! fails with the error a server would give.
//!
//! auth/verify -m [-s modules] [-r revoked] [-t time] module: check a
//! module's signatures (`docs/signing.md`) against the module signer list
//! (default `/lib/keys/modules`) and the revocation list. Prints the
//! module's digest and ABI version and, for each signature, the signing
//! key, its standing and the names in its certificates, and fails with
//! the error the loader would give under `require`.

use authlib::fmt::{hex, keys, modsig, proof};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/verify [-m] [-s signers] [-r revoked] [-u name] [-t time] file";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signers = None;
    let mut module = false;
    let mut revoked = String::from(authlib::REVOKED);
    let mut name = None;
    let mut now = authlib::now();
    while let Some(c) = arg.opt() {
        match c {
            'm' => module = true,
            's' => signers = Some(arg.earg()?),
            'r' => revoked = arg.earg()?,
            'u' => name = Some(arg.earg()?),
            't' => now = authlib::expiry(&arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [file] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let text = std::fs::read(file).map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
    let signers = signers.unwrap_or_else(|| {
        String::from(if module {
            authlib::MODULES
        } else {
            authlib::SIGNERS
        })
    });
    let signers = std::fs::read(&signers)
        .map(|t| keys::parse_list_lenient(&t))
        .unwrap_or_default();
    let revoked = match std::fs::read(&revoked) {
        Ok(t) => keys::parse_list(&t),
        Err(_) => Some(Vec::new()),
    };
    if module {
        let trust = modsig::Trust {
            anchors: &signers,
            revoked: revoked.as_deref(),
            now,
        };
        return verify_module(file, &text, &trust);
    }
    let cert = keys::find_cert(&text).ok_or_else(|| Error(format!("{file}: bad certificate")))?;
    let policy = proof::Policy {
        keys: &[],
        signers: &signers,
        revoked: revoked.as_deref(),
        now,
    };
    println!("name {}", cert.name);
    println!("key {}", hex::encode(&cert.key));
    println!("expires {}", authlib::show_expiry(cert.expires));
    for (s, _) in &cert.sigs {
        let standing = if !cert.signed_by(s) {
            "bad"
        } else if revoked.as_ref().is_some_and(|r| r.contains(s)) {
            "revoked"
        } else if signers.contains(s) {
            "trusted"
        } else {
            "untrusted"
        };
        println!("signer {} {standing}", hex::encode(s));
    }
    proof::check_cert(&policy, &cert, name.as_deref()).map_err(Error::from)
}

fn verify_module(file: &str, wasm: &[u8], trust: &modsig::Trust<'_>) -> Result<(), Error> {
    let info = modsig::inspect(wasm)
        .map_err(|_| Error(format!("{file}: {}", purgatorio::abi::error::EBADEXEC)))?;
    println!("digest {}", hex::encode(&info.digest));
    match info.abi {
        [] => println!("abi -"),
        abi => println!("abi {}", String::from_utf8_lossy(abi)),
    }
    if let (Ok(entries), Some(msg)) = (
        modsig::entries(&info),
        modsig::message(&info.digest, info.abi),
    ) {
        for e in &entries {
            let mut line = format!(
                "signer {} {}",
                hex::encode(e.key()),
                modsig::standing(e, &msg, trust).as_str()
            );
            for c in e.certs() {
                line.push(' ');
                line.push_str(&c.name);
            }
            println!("{line}");
        }
    }
    modsig::check(&info, trust).map(drop).map_err(Error::from)
}
