//! kill [-g] pid|module ...: interrupt progs, or their whole groups with
//! `-g`.
//!
//! An argument that starts with a digit is a pid; any other is a module
//! name, and every prog running that module is killed, its pid printed.
//! As in Inferno's kill, a failure is reported and kill goes on with the
//! rest of the list. The status is `fail:nothing killed` only when nothing
//! was, and `fail: usage` for a bad command line.

use std::io::Write;

use purgatorio::{Context, Error};

const THREAD_EXITED: &str = "thread exited";

fn usage() -> ! {
    eprintln!("usage: kill [-g] pid|module ...");
    purgatorio::exits("fail: usage");
}

/// Writes `msg` to `/prog/<pid>/ctl`. Errors are `Err((what, errstr))`,
/// `what` being the message prefix kill reports them with.
fn kill(pid: &str, msg: &str) -> Result<(), (String, String)> {
    let path = format!("/prog/{pid}/ctl");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| (format!("cannot open {path}"), Error::from(e).0))?;
    f.write_all(msg.as_bytes())
        .map_err(|e| (format!("cannot kill {pid}"), Error::from(e).0))
}

/// Kills every prog whose module is `name`, printing their pids. Returns
/// how many were killed.
fn kill_module(name: &str, msg: &str) -> usize {
    let entries = match std::fs::read_dir("/prog") {
        Ok(e) => e,
        Err(e) => {
            eprintln!("kill: open /prog: {}", Error::from(e));
            return 0;
        }
    };
    let mut pids: Vec<u32> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse().ok())
        .collect();
    pids.sort();
    let me = purgatorio::pid();
    let mut found = false;
    let mut killed = Vec::new();
    for pid in pids {
        if pid == me {
            continue;
        }
        let path = format!("/prog/{pid}/status");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                // A prog that exits after the directory is read is gone.
                let e = Error::from(e).0;
                if e != THREAD_EXITED && !e.ends_with("does not exist") {
                    eprintln!("kill: cannot read {path}: {e}");
                }
                continue;
            }
        };
        let f: Vec<&str> = text.split_whitespace().collect();
        if f.len() < 6 || f[5..].join(" ") != name {
            continue;
        }
        found = true;
        match kill(&pid.to_string(), msg) {
            Ok(()) => killed.push(pid.to_string()),
            Err((_, e)) if e == THREAD_EXITED || e.ends_with("does not exist") => {}
            Err((what, e)) => eprintln!("kill: {what}: {e}"),
        }
    }
    if !found {
        eprintln!("kill: cannot find {name}");
    }
    if !killed.is_empty() {
        println!("{}", killed.join(" "));
    }
    killed.len()
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut group = false;
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        for c in flags.chars() {
            match c {
                'g' => group = true,
                _ => usage(),
            }
        }
        i += 1;
    }
    let targets = &args[i..];
    if targets.is_empty() || targets.iter().any(|t| t.is_empty()) {
        usage();
    }
    let msg = if group { "killgrp" } else { "kill" };
    let mut killed = 0;
    for t in targets {
        if t.starts_with(|c: char| c.is_ascii_digit()) {
            match kill(t, msg) {
                Ok(()) => killed += 1,
                Err((what, e)) => eprintln!("kill: {what}: {e}"),
            }
        } else {
            killed += kill_module(t, msg);
        }
    }
    if killed == 0 {
        purgatorio::exits("fail:nothing killed");
    }
    Ok(())
}
