//! auth/ai2key [-t tag] keyfile ...: print each authinfo as the line that
//! loads it into `auth/factotum`'s `ctl`, as ai2key(8) does for Inferno's
//! authinfo and factotum's `infauth` keys:
//!
//! ```text
//! key proto=ed25519 [tag] user=<name> !seed=<hex> cert=<pgcert1 line>
//! ```
//!
//! `tag` (usually attribute comments) is printed after the `proto`
//! attribute. A `keyfile` without a `/` is a name under
//! `/usr/<user>/keyring`, as for `getauthinfo`. The output can be kept
//! in a secure store and loaded later:
//! `auth/secstore -G factotum > /mnt/factotum/ctl`.
//!
//! Adapted: Inferno's authinfo held a key pair of its own algorithms, a
//! certificate and Diffie-Hellman parameters, and its line was for the
//! `infauth` protocol. Here an authinfo is an Ed25519 seed and a
//! `pgcert1` certificate (`docs/auth.md`), so the line is factotum's
//! `ed25519` key. There is no `server`, `dom` or `service` attribute:
//! factotum here picks `ed25519` keys by `user` and `role`.

use authlib::fmt::{attr::quote, hex};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/ai2key [-t tag] keyfile ...";

fn line(path: &str, tag: Option<&str>) -> Result<String, Error> {
    let info = purgatorio::auth::read_authinfo(path)?;
    let tag = tag.map(|t| format!(" {t}")).unwrap_or_default();
    Ok(format!(
        "key proto=ed25519{tag} user={} !seed={} cert={}\n",
        quote(&info.cert.name),
        hex::encode(&info.seed),
        quote(&info.cert.encode())
    ))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // As Inferno's ai2key: any failure is `fail:error`.
    run(args).map_err(|e| e.fails_as("fail:error"))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut tag = None;
    while let Some(c) = arg.opt() {
        match c {
            't' => tag = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let files = arg.argv();
    if files.is_empty() {
        return Err(usage);
    }
    let user = purgatorio::auth::user();
    let mut out = String::new();
    for f in &files {
        out.push_str(&line(
            &purgatorio::auth::keyfile_path(f, &user),
            tag.as_deref(),
        )?);
    }
    print!("{out}");
    Ok(())
}
