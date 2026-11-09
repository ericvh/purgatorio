//! kill [-g] pid...: interrupt a prog, or its whole group with -g.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut group = false;
    let mut pids = Vec::new();
    for a in args.iter().skip(1) {
        if a == "-g" {
            group = true;
        } else {
            pids.push(a.parse::<u32>().map_err(|_| Error::from("bad pid"))?);
        }
    }
    if pids.is_empty() {
        return Err(Error::from("usage: kill [-g] pid..."));
    }
    let cmd = if group { "killgrp\n" } else { "kill\n" };
    for pid in pids {
        std::fs::write(format!("/prog/{pid}/ctl"), cmd).map_err(|_| Error::last())?;
    }
    Ok(())
}
