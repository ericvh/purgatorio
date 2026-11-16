//! auth/modpolicy [-a ed25519|hybrid] warn|require cmd [arg ...]: set this
//! prog's module policy (`docs/signing.md`), then run `cmd` under it. The
//! command and everything it starts inherit the policy and cannot lower
//! it. The trust lists are the `/lib/keys/modules` and `/lib/keys/revoked`
//! this prog sees when it starts. `-a hybrid` counts only hybrid Ed25519 +
//! ML-DSA-65 signatures; `-a ed25519`, the default, counts either kind.
//!
//! Exits with the command's status.

use purgatorio::{Context, Error};

const USAGE: &str = "usage: auth/modpolicy [-a ed25519|hybrid] warn|require cmd [arg ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (hybrid, rest) = match args.get(1).map(String::as_str) {
        Some("-a") => match args.get(2).map(String::as_str) {
            Some("hybrid") => (true, &args[3..]),
            Some("ed25519") => (false, &args[3..]),
            _ => return Err(Error::from(USAGE)),
        },
        _ => (false, &args[1..]),
    };
    let (level, argv) = match rest {
        [level, argv @ ..] if !argv.is_empty() && (level == "warn" || level == "require") => {
            (level, argv)
        }
        _ => return Err(Error::from(USAGE)),
    };
    let ctl = format!("/prog/{}/ctl", purgatorio::pid());
    let policy = if hybrid {
        format!("modules {level} hybrid")
    } else {
        format!("modules {level}")
    };
    purgatorio::fs::write(&ctl, policy).map_err(|e| Error(format!("{ctl}: {}", Error::from(e))))?;
    let path =
        purgatorio::find_command(&argv[0]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
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
