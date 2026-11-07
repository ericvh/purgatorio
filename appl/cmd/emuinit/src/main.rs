//! emuinit [cmd [args ...]]: the emu's first prog. With a command it runs it
//! in this same prog; until `sh` exists (M2) it otherwise prints a usage hint.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1) {
        Some(cmd) => Err(purgatorio::exec(cmd, &args[1..])),
        None => {
            eprintln!("emuinit: no command; run `emu cmd [args...]` (sh arrives in M2)");
            Ok(())
        }
    }
}
