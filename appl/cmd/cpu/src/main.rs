//! cpu [-A] [-k keyfile] [-C alg] [net!]host [command [arg...]]: run
//! `command` (default `sh`) on `host` with this system's devices. It is
//! `rcmd` with `auxi/cpuslave` as the remote command: the slave binds
//! this prog's `/dev` (seen there as `/n/client/dev`) before the remote
//! `/dev`, so the command's console and the other device files are the
//! caller's, and then runs the command. Standard input, output and error
//! and the exit status are carried as `rcmd` carries them.
//!
//! `-A` and `-k` are as for `rcmd`. `-C` names an algorithm for the
//! connection; the only one here is `none`.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use rstyx::client::{self, Opts};
use rstyx::proto::quote;

const USAGE: &str = "cpu [-A] [-k keyfile] [-C alg] [net!]host [command [arg...]]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some(rstyx::io::PUMP) {
        rstyx::io::pump();
        return Ok(());
    }
    let mut arg = Arg::new(args, USAGE);
    let mut o = Opts {
        module: "/bin/cpu.wasm",
        ..Opts::default()
    };
    while let Some(c) = arg.opt() {
        match c {
            'A' => o.noauth = true,
            'k' | 'f' => o.keyfile = Some(arg.earg()?),
            'C' => {
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
    let mut line = String::from("auxi/cpuslave");
    for word in cmd {
        line.push(' ');
        line.push_str(&quote(word));
    }
    let status = client::run(&o, host, &line)?;
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}
