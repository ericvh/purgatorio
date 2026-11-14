//! rcmd [-AS] [-k keyfile] [-e alg] [-x dir] host [cmd [arg...]]: run `cmd` (default
//! `sh`) on `host`, where `auxi/rstyxd` serves the `rstyx` service. The
//! command's standard input, output and error are this prog's, this
//! prog's namespace appears there as `/n/client`, and its exit status is
//! this prog's; `-x dir` shows it `dir` instead of `/`. A single `cmd` is a command line for the remote `sh`;
//! several words are one command and arrive as they were given.
//!
//! The connection is authenticated as a mount is (`docs/auth.md`), with
//! the authinfo `keyfile` if given (`-f` is Inferno's name for `-k`), and
//! then encrypted; `-A` skips both. `-S` requires the server to prove
//! itself with a key or certificate this namespace trusts as a server's.
//! `-e` names the algorithm: the one cipher, `chacha20poly1305`, or
//! `none` with `-A`. The protocol is in `docs/rcmd.md`.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use rstyx::client::{self, Opts};

const USAGE: &str = "rcmd [-AS] [-k keyfile] [-e alg] [-x dir] host [cmd [arg...]]";

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
    let mut alg = None;
    while let Some(c) = arg.opt() {
        match c {
            'A' => o.noauth = true,
            'S' => o.srvauth = true,
            'k' | 'f' => o.keyfile = Some(arg.earg()?),
            'x' => o.export = Some(arg.earg()?),
            'e' => alg = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    if let Some(alg) = alg {
        o.pq = client::check_alg(&alg, o.noauth)?;
    }
    let rest = arg.argv();
    if o.noauth && o.srvauth {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let Some((host, cmd)) = rest.split_first() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let status = client::run(&o, host, &rstyx::proto::command(cmd))?;
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}
