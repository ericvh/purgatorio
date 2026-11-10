//! rcmd [-A] [-k keyfile] [-e alg] host [cmd [arg...]]: run `cmd` (default
//! `sh`) on `host`, where `auxi/rstyxd` serves the `rstyx` service. The
//! command's standard input, output and error are this prog's, this
//! prog's namespace appears there as `/n/client`, and its exit status is
//! this prog's. A single `cmd` is a command line for the remote `sh`;
//! several words are one command and arrive as they were given.
//!
//! The connection is authenticated as a mount is (`docs/auth.md`), with
//! the authinfo `keyfile` if given (`-f` is Inferno's name for `-k`); `-A`
//! skips that. `-e` names an algorithm for the connection; the only one
//! here is `none`. The protocol is in `docs/rcmd.md`.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use rstyx::client::{self, Opts};

const USAGE: &str = "rcmd [-A] [-k keyfile] [-e alg] host [cmd [arg...]]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some(rstyx::io::PUMP) {
        rstyx::io::pump();
        return Ok(());
    }
    let mut arg = Arg::new(args, USAGE);
    let mut o = Opts {
        module: "/bin/rcmd.wasm",
        ..Opts::default()
    };
    while let Some(c) = arg.opt() {
        match c {
            'A' => o.noauth = true,
            'k' | 'f' => o.keyfile = Some(arg.earg()?),
            'e' => {
                let alg = arg.earg()?;
                if alg != "none" && alg != "clear" {
                    return Err(Error(format!("unknown algorithm {alg}")));
                }
            }
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let Some((host, cmd)) = rest.split_first() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let status = client::run(&o, host, &rstyx::proto::command(cmd))?;
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}
