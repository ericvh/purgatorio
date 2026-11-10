//! auth/verify [-s signers] [-r revoked] [-u name] [-t time] file: check the
//! certificate in `file` (a certificate or an authinfo) as a server would,
//! against a signer list (default `/lib/keys/signers`) and a revocation
//! list (default `/lib/keys/revoked`) at `time` (default now). Prints the
//! certificate's name, key, expiry and each signer with its standing, and
//! fails with the error a server would give.

use authlib::fmt::{hex, keys, proof};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/verify [-s signers] [-r revoked] [-u name] [-t time] file";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut signers = String::from(authlib::SIGNERS);
    let mut revoked = String::from(authlib::REVOKED);
    let mut name = None;
    let mut now = authlib::now();
    while let Some(c) = arg.opt() {
        match c {
            's' => signers = arg.earg()?,
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
    let cert = keys::find_cert(&text).ok_or_else(|| Error(format!("{file}: bad certificate")))?;
    let signers = std::fs::read(&signers)
        .map(|t| keys::parse_list_lenient(&t))
        .unwrap_or_default();
    let revoked = match std::fs::read(&revoked) {
        Ok(t) => keys::parse_list(&t),
        Err(_) => Some(Vec::new()),
    };
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
