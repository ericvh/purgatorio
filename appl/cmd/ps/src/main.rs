//! ps: list progs from /prog, as Inferno's ps(1): each prog's `status`
//! line as prog(3) gives it (pid, process group, user, CPU time, state,
//! memory, module), in pid order.

use purgatorio::println;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    let mut pids: Vec<u32> = purgatorio::fs::read_dir("/prog")
        .map_err(|e| Error(format!("ps: cannot open /prog: {}", Error::from(e))))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse().ok())
        .collect();
    pids.sort();
    for pid in pids {
        // A prog that exits after the directory is read is not listed, as
        // in Inferno's ps.
        if let Ok(text) = purgatorio::fs::read_to_string(format!("/prog/{pid}/status")) {
            println!("{text}");
        }
    }
    Ok(())
}
