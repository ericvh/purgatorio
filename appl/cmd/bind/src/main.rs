//! bind [-abrc] source target: make `target` refer to `source` in this
//! namespace, which is shared with the shell that ran it. `-b` puts
//! `source` before the existing `target` in a union, `-a` after it, and
//! `-r` (the default) replaces it; `-c` lets files be created in
//! `source` through the union.

use purgatorio::abi::mount;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "bind [-abrc] source target");
    let (mut order, mut create) = (mount::MREPL, 0);
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'r' => order = mount::MREPL,
            'c' => create = mount::MCREATE,
            _ => return Err(arg.usage()),
        }
    }
    let [source, target] = arg.argv().try_into().map_err(|_| arg_usage())?;
    if let Err(e) = purgatorio::bind(&source, &target, order | create) {
        // As Inferno's bind: `fail:bind`.
        eprintln!("bind: cannot bind {source} onto {target}: {e}");
        purgatorio::exits("fail:bind");
    }
    Ok(())
}

fn arg_usage() -> Error {
    Error::from("usage: bind [-abrc] source target")
}
