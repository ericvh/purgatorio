//! emuinit [cmd [args ...]]: the emu's first prog. It runs `sh -l`, or the
//! given command, as a child prog and exits with the child's status.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let argv = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        vec![String::from("sh"), String::from("-l")]
    };
    let pid = purgatorio::spawn(&argv[0], &argv, 0, &[])?;
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
