//! broke [user]: print sh(1) commands that kill the broken progs of
//! `user` (default: the current user), as Inferno's `broke` does. It
//! kills nothing itself: `broke | sh` does that.
//!
//! Inferno's broke is a script that edits ps(1)'s output into one line
//! per broken prog: `echo kill>/prog/pid/ctl # ` and the prog's ps line.
//! This prints the same, from our ps's line.
//!
//! A prog is broken when it trapped while traced (`docs/debug.md`): it
//! stays in `/prog`, holding only its last stack, until it is killed.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let user = match args.as_slice() {
        [_] => std::fs::read_to_string("/dev/user").map_err(|_| Error::last())?,
        [_, user] if !user.starts_with('-') => user.clone(),
        _ => return Err(Error::from("usage: broke [user]")),
    };
    let user = user.trim();
    let mut pids: Vec<u32> = std::fs::read_dir("/prog")?
        .filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse().ok())
        .collect();
    pids.sort();
    for pid in pids {
        // A prog may have gone since the directory was read.
        let Ok(status) = std::fs::read_to_string(format!("/prog/{pid}/status")) else {
            continue;
        };
        // pid ppid pgrp user state name. Only a prog with an exception is
        // broken for good: one that is merely being killed says `broken`
        // too, on its way out.
        let f: Vec<&str> = status.split_whitespace().collect();
        if f.len() < 6 || f[3] != user || f[4] != "broken" {
            continue;
        }
        let trapped =
            std::fs::read_to_string(format!("/prog/{pid}/exception")).is_ok_and(|e| !e.is_empty());
        if trapped {
            // The line as ps prints it.
            println!(
                "echo kill>/prog/{pid}/ctl # {:>5} {:>5} {:>5} {:<10} {:<8} {}",
                f[0],
                f[1],
                f[2],
                f[3],
                f[4],
                f[5..].join(" ")
            );
        }
    }
    Ok(())
}
