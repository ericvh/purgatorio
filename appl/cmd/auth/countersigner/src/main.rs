//! auth/countersigner [-f key] file: add the signature of `key` (a seed
//! file, default `/keydb/signerkey`) to the certificate in `file` (a
//! certificate or an authinfo), and print the file with the new
//! certificate in place. A server that trusts either signer then accepts
//! it.

use authlib::fmt::keys;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/countersigner [-f key] file";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut keyfile = String::from(authlib::SIGNERKEY);
    while let Some(c) = arg.opt() {
        match c {
            'f' => keyfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [file] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let key = purgatorio::auth::read_seed(&keyfile)?;
    let text =
        std::fs::read_to_string(file).map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
    let mut cert = keys::find_cert(text.as_bytes())
        .ok_or_else(|| Error(format!("{file}: bad certificate")))?;
    let old = cert.encode();
    cert.sign(&key).map_err(Error::from)?;
    let mut out = String::new();
    for line in text.lines() {
        if line == old {
            out.push_str(&cert.encode());
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}
