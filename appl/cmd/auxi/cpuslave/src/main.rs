//! auxi/cpuslave [command [arg...]]: the remote half of `cpu`, started by
//! `auxi/rstyxd` with the caller's namespace on `/n/client`. In a copy of
//! the session's namespace, it binds the caller's `/dev` before this
//! system's, so that the console and the
//! other device files the caller has are the ones `command` (default
//! `sh`) opens, runs the command with the standard input, output and
//! error it was given, and exits with the command's status. One word is a
//! command line for `sh`; several are one command.

use purgatorio::abi::mount::MBEFORE;
use purgatorio::abi::pctl::{FORKNS, NEWFD};
use purgatorio::{Context, Error};

const CLIENT: &str = "/n/client/dev";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let words = &args[1..];
    if words.first().is_some_and(|w| w.starts_with('-')) {
        return Err(Error::from("usage: auxi/cpuslave [command [arg...]]"));
    }
    purgatorio::stat(CLIENT).map_err(|e| Error(format!("{CLIENT}: {e}")))?;
    purgatorio::pctl(FORKNS, &[])?;
    purgatorio::bind(CLIENT, "/dev", MBEFORE).map_err(|e| Error(format!("bind /dev: {e}")))?;
    let argv: Vec<String> = match words {
        [] => vec![String::from("sh")],
        [line] => ["sh", "-c", line].map(String::from).to_vec(),
        words => words.to_vec(),
    };
    let pid = purgatorio::spawn(&argv[0], &argv, NEWFD, &[(0, 0), (1, 1), (2, 2)])
        .map_err(|e| Error(format!("{}: {e}", argv[0])))?;
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
