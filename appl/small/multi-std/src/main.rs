//! A multi-call binary of `std` commands, busybox style: sixteen of the
//! small commands in `appl/cmd` in one module, chosen by the name it is
//! run as. It exists to measure what sharing Rust `std` and the SDK
//! between commands saves (docs/footprint.md); `build.rs` makes the
//! modules from the commands' own sources.

use purgatorio::{Context, Error};

include!(concat!(env!("OUT_DIR"), "/cmds.rs"));

#[purgatorio::main]
fn main(ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let argv0 = args.first().cloned().unwrap_or_default();
    let name = argv0.rsplit('/').next().unwrap_or(&argv0);
    let name = name.strip_suffix(".wasm").unwrap_or(name);
    dispatch(name, ctx, args)
}
