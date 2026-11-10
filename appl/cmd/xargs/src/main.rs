//! xargs command [arg ...]: for each line of standard input, run `command`
//! with the given arguments followed by the words of that line (split at
//! blanks and tabs). Empty lines are skipped. The commands run one after
//! another; if any fails, xargs exits with the last failure.

use std::io::{self, BufRead};

use purgatorio::{Context, Error};

/// Finds `name` as the shell would: a path as given or with `.wasm`, or
/// a bare name in /bin.
fn resolve(name: &str) -> Result<String, Error> {
    let candidates = if name.starts_with('/') || name.starts_with("./") || name.starts_with("../") {
        vec![String::from(name), format!("{name}.wasm")]
    } else {
        vec![format!("/bin/{name}.wasm")]
    };
    for c in &candidates {
        if purgatorio::stat(c).is_ok_and(|d| !d.qid.is_dir()) {
            return Ok(c.clone());
        }
    }
    Err(Error(format!(
        "can't load {name}: {}",
        purgatorio::abi::error::ENONEXIST
    )))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        return Err(Error::from(
            "usage: xargs command [command args] <[list of last command arg]",
        ));
    }
    let cmd = &args[1];
    let path = resolve(cmd)?;
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
