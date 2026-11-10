//! auth/modpolicy warn|require cmd [arg ...]: set this prog's module
//! policy (`docs/signing.md`), then run `cmd` under it. The command and
//! everything it starts inherit the policy and cannot lower it. The trust
//! lists are the `/lib/keys/modules` and `/lib/keys/revoked` this prog
//! sees when it starts.
//!
//! Exits with the command's status.

use purgatorio::{Context, Error};

const USAGE: &str = "usage: auth/modpolicy warn|require cmd [arg ...]";

/// Finds `name` as the shell would: a path as given or with `.wasm`, or
/// a bare name in /bin.
fn resolve(name: &str) -> Result<String, Error> {
    let candidates = if name.starts_with('/') || name.starts_with("./") || name.starts_with("../") {
        vec![String::from(name), format!("{name}.wasm")]
    } else {
        vec![format!("/bin/{name}.wasm")]
    };
    let mut err = String::from(purgatorio::abi::error::ENONEXIST);
    for c in &candidates {
        match purgatorio::stat(c) {
            Ok(d) if !d.qid.is_dir() => return Ok(c.clone()),
            Ok(_) => err = String::from(purgatorio::abi::error::EISDIR),
            Err(e) => err = e.0,
        }
    }
    Err(Error(err))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (level, argv) = match args.as_slice() {
        [_, level, argv @ ..] if !argv.is_empty() && (level == "warn" || level == "require") => {
            (level, argv)
        }
        _ => return Err(Error::from(USAGE)),
    };
    let ctl = format!("/prog/{}/ctl", purgatorio::pid());
    std::fs::write(&ctl, format!("modules {level}"))
        .map_err(|e| Error(format!("{ctl}: {}", Error::from(e))))?;
    let path = resolve(&argv[0]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
    // spawn's error already names the module.
    let pid = purgatorio::spawn(&path, argv, 0, &[])?;
    loop {
        let w = purgatorio::wait()?;
        if w.pid == pid {
            if !w.status.is_empty() {
                purgatorio::exits(&w.status);
            }
            return Ok(());
        }
    }
}
