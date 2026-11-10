//! ndb/regquery [-n] [-m mntpt] [attr value ...]: print the services in
//! the registry at `mntpt` (default `/mnt/registry`) that have all the
//! given pairs (a value of `*` matches anything), one per line; with `-n`
//! only their names. With no pairs it prompts with `> ` and reads one
//! query per line from standard input.

use std::fs::OpenOptions;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};

use ndb::quote;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "regquery [-n] [-m mntpt] [attr value ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut names = false;
    let mut mnt = String::from("/mnt/registry");
    while let Some(c) = arg.opt() {
        match c {
            'n' => names = true,
            'm' => mnt = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let q = arg.argv();
    if !q.is_empty() {
        let q: Vec<String> = q.iter().map(|w| quote(w)).collect();
        return query(&mnt, &q.join(" "), names);
    }
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            println!();
            return Ok(());
        }
        if let Err(e) = query(&mnt, line.trim(), names) {
            eprintln!("regquery: {e}");
        }
    }
}

fn query(mnt: &str, q: &str, names: bool) -> Result<(), Error> {
    let err = |_| Error::last();
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{mnt}/find"))
        .map_err(err)?;
    if !q.is_empty() {
        f.write_all(q.as_bytes()).map_err(err)?;
    }
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(err)?;
    for l in text.lines() {
        if names {
            println!("{}", ndb::words(l).first().map_or("", String::as_str));
        } else {
            println!("{l}");
        }
    }
    Ok(())
}
