//! xargs command [arg ...]: for each line of standard input, run `command`
//! with the given arguments followed by the words of that line (split at
//! blanks and tabs). Empty lines are skipped. The commands run one after
//! another; if any fails, xargs exits with the last failure.

use std::io::{self, BufRead};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // As Inferno's xargs: a bad command line or a command it cannot load
    // is reported with no status.
    if args.len() < 2 {
        eprintln!("Usage: xargs command [command args] <[list of last command arg]");
        return Ok(());
    }
    let cmd = &args[1];
    let Ok(path) = purgatorio::find_command(cmd) else {
        eprintln!(
            "xargs: can't load {cmd}: {}",
            purgatorio::abi::error::ENONEXIST
        );
        return Ok(());
    };
    let mut status = String::new();
    let mut input = io::stdin().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let words: Vec<String> = text
            .split([' ', '\t', '\n'])
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect();
        if words.is_empty() {
            continue;
        }
        let mut argv = args[1..].to_vec();
        argv.extend(words);
        let pid = purgatorio::spawn(&path, &argv, 0, &[])?;
        loop {
            let w = purgatorio::wait()?;
            if w.pid == pid {
                if !w.status.is_empty() {
                    status = w.status;
                }
                break;
            }
        }
    }
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}
