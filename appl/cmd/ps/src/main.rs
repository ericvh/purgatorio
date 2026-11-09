//! ps: list progs from /prog.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    println!(
        "{:>5} {:>5} {:>5} {:<10} {:<8} name",
        "pid", "ppid", "pgrp", "user", "state"
    );
    let mut pids: Vec<u32> = std::fs::read_dir("/prog")?
        .filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse().ok())
        .collect();
    pids.sort();
    for pid in pids {
        let text = std::fs::read_to_string(format!("/prog/{pid}/status"))?;
        let f: Vec<&str> = text.split_whitespace().collect();
        if f.len() >= 6 {
            println!(
                "{:>5} {:>5} {:>5} {:<10} {:<8} {}",
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
