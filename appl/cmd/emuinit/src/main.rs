//! emuinit [cmd [args ...]]: the emu's first prog. With a command it runs it
//! as a child prog and exits with the child's status; until `sh` exists it
//! otherwise prints a usage hint.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let Some(cmd) = args.get(1) else {
        eprintln!("emuinit: no command; run `emu cmd [args...]` (sh arrives in M2)");
        return Ok(());
    };
    let pid = purgatorio::spawn(cmd, &args[1..], 0, &[])?;
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
