//! runas user cmd [arg ...]: run `cmd` as `user` (Inferno's runas(1)).
//!
//! The prog takes a namespace, environment and process group of its own,
//! so the command's binds, its variables and a `killgrp` stay away from
//! the caller's, then writes `user` to `/dev/user` and runs `cmd`. Only
//! the host owner may do so (`docs/auth.md`, "Users"): anyone else gets
//! `permission denied`. The command and everything it starts act as
//! `user` and cannot change it back.
//!
//! Exits with the command's status.

use purgatorio::{Context, Error, abi};

const USAGE: &str = "usage: runas user cmd [arg ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (user, argv) = match args.as_slice() {
        [_, user, argv @ ..] if !argv.is_empty() => (user, argv),
        _ => return Err(Error::from(USAGE)),
    };
    use abi::pctl::{FORKENV, FORKNS, NEWPGRP};
    purgatorio::pctl(FORKNS | FORKENV | NEWPGRP, &[])?;
    std::fs::write("/dev/user", user)
        .map_err(|e| Error(format!("can't set user name: {}", Error::from(e))))?;
    let path =
        purgatorio::find_command(&argv[0]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
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
