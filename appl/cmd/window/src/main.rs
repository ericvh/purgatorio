//! window [-r minx miny maxx maxy] [-dx n] [-dy n] [-minx n] [-miny n]
//! [-hide] [-scroll | -noscroll] [-cd dir] [cmd arg ...]: make a new
//! window in the rio this prog runs under, as Plan 9's window(1). It
//! writes `new`, the options and the command to `/dev/wctl` (rio(4));
//! with no command the window runs a shell. rio checks the request, and
//! its error is window's. Plan 9's `-m` (a window in another rio, through
//! `/srv/rio`) is not taken.

use std::io::Write;

use purgatorio::{Context, Error};

const USAGE: &str = "usage: window [-r minx miny maxx maxy] [-dx n] [-dy n] [-minx n] [-miny n] [-hide] [-scroll|-noscroll] [-cd dir] [cmd arg ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut req = String::from("new");
    let mut rest = args.iter().skip(1).peekable();
    while let Some(o) = rest.peek() {
        let n = match o.as_str() {
            "-r" => 4,
            "-dx" | "-dy" | "-minx" | "-miny" => 1,
            "-hide" | "-scroll" | "-noscroll" => 0,
            "-cd" => {
                rest.next();
                match rest.next() {
                    // rio's request is split at white space.
                    Some(d) if !d.is_empty() && !d.contains(char::is_whitespace) => {
                        req.push_str(" -cd ");
                        req.push_str(d);
                    }
                    _ => usage(),
                }
                continue;
            }
            "--" => {
                rest.next();
                break;
            }
            _ => break,
        };
        req.push(' ');
        req.push_str(rest.next().map_or("", String::as_str));
        for _ in 0..n {
            match rest.next() {
                Some(v) if v.parse::<i32>().is_ok() => {
                    req.push(' ');
                    req.push_str(v);
                }
                _ => usage(),
            }
        }
    }
    let cmd: Vec<&str> = rest.map(String::as_str).collect();
    if cmd.first().is_some_and(|c| c.starts_with('-')) {
        usage();
    }
    if !cmd.is_empty() {
        req.push(' ');
        req.push_str(&cmd.join(" "));
    }
    let mut wctl = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/wctl")
        .unwrap_or_else(|e| {
            eprintln!("window: can't open /dev/wctl: {}", Error::from(e));
            purgatorio::exits("open");
        });
    if let Err(e) = wctl.write_all(req.as_bytes()) {
        eprintln!("window: {}", Error::from(e));
        purgatorio::exits("wctl");
    }
    Ok(())
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    purgatorio::exits("usage");
}
