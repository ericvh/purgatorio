//! trace command [arg ...]: run a command with tracing on
//! (`docs/debug.md`), so that it and every prog it starts keep a call
//! stack that `stack` can print while they run.
//!
//! A traced prog that traps does not go away: it stays broken, as a prog
//! does in Inferno, with the stack it trapped with, until it is killed
//! (`kill`, or `broke` for all of them). When the command itself breaks,
//! `trace` says so on standard error, as `trace: <pid> "<name>": broken:
//! <status>`. The exit status is the command's.
//!
//! Tracing is a debugging mode: a traced prog runs somewhat slower and
//! can nest calls half as deep.

use purgatorio::{Context, Error};

const USAGE: &str = "usage: trace command [arg ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let argv = &args[1.min(args.len())..];
    if argv.is_empty() || argv[0].starts_with('-') {
        return Err(Error::from(USAGE));
    }
    let ctl = format!("/prog/{}/ctl", purgatorio::pid());
    std::fs::write(&ctl, "trace on").map_err(|e| Error(format!("{ctl}: {}", Error::from(e))))?;
    let path =
        purgatorio::find_command(&argv[0]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
    // spawn's error already names the module.
    let pid = purgatorio::spawn(&path, argv, 0, &[])?;
    loop {
        let w = purgatorio::wait()?;
        if w.pid != pid {
            continue;
        }
        let broken =
            std::fs::read_to_string(format!("/prog/{pid}/exception")).is_ok_and(|e| !e.is_empty());
        if broken {
            eprintln!("trace: {pid} \"{}\": broken: {}", w.name, w.status);
        }
        if !w.status.is_empty() {
            purgatorio::exits(&w.status);
        }
        return Ok(());
    }
}
