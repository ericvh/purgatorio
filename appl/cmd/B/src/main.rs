//! B [-line] file ...: send files to the editor. Each file, made absolute
//! from the current directory and followed by `:line` when a `-line`
//! option came before it, goes to the plumber as a message for the
//! `edit` port (from `B`, of type `text`); acme reads that port and
//! opens the file in a window, at the line.
//!
//! Inferno's B ran the host's B through `/cmd`, to reach an editor on
//! the host; this one reaches acme through the plumber, as Plan 9's
//! does. The plumber must be running (`plumber`), with a rule that
//! names the `edit` port.

#![allow(non_snake_case)]

use std::io::Write;

use plumbmsg::Msg;
use purgatorio::abi::mode;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        eprintln!("Usage: B [-line] file ...");
        purgatorio::exits("fail:usage");
    }
    let dir = purgatorio::dot().unwrap_or_default();
    let mut line: Option<u32> = None;
    let mut msgs = Vec::new();
    for a in &args[1..] {
        if let Some(n) = a.strip_prefix('-').and_then(|n| n.parse::<u32>().ok()) {
            line = Some(n);
            continue;
        }
        let mut name = if a.starts_with('/') || dir.is_empty() {
            a.clone()
        } else {
            format!("{}/{a}", dir.trim_end_matches('/'))
        };
        if let Some(n) = line {
            name.push_str(&format!(":{n}"));
        }
        msgs.push(Msg {
            src: String::from("B"),
            dst: String::from("edit"),
            dir: dir.clone(),
            kind: String::from("text"),
            attr: String::new(),
            data: name.into_bytes(),
        });
    }
    if msgs.is_empty() {
        eprintln!("Usage: B [-line] file ...");
        purgatorio::exits("fail:usage");
    }
    let fd = purgatorio::open(plumbmsg::INPUT, mode::OWRITE)
        .map_err(|e| Error(format!("can't open {}: {e}", plumbmsg::INPUT)))?;
    let mut f = std::fs::File::from(fd);
    for m in msgs {
        if f.write_all(&m.pack()).is_err() {
            return Err(Error(format!(
                "{}: {}",
                String::from_utf8_lossy(&m.data),
                purgatorio::errstr()
            )));
        }
    }
    Ok(())
}
