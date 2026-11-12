//! dbm/store dbfile [key value]: store `value` under `key` in the dbm
//! database `dbfile` (`dbfile.dir` and `dbfile.pag`, which must be there),
//! or with no key, one pair from each line of standard input: the key is
//! the line up to its first space or tab, the value the rest. A key that
//! is there already is reported and left alone, and the status is then
//! `fail:store`. A line with no space or tab ends the input as an error.

use std::io::BufRead;

use dbm::{Db, Stored};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (file, pair) = match args.as_slice() {
        [_, f] => (f, None),
        [_, f, k, v] => (f, Some((k.clone(), v.clone()))),
        _ => return Err(Error::from("usage: dbm/store dbfile [key value]")),
    };
    let mut db = match Db::open(file, false) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("dbm/store: {file}: {}", Error::from(e));
            purgatorio::exits("fail:open");
        }
    };
    let mut failed = false;
    let mut put = |k: &[u8], v: &[u8]| match db.store(k, v, false) {
        Ok(Stored::Done) => {}
        Ok(Stored::Exists) => {
            eprintln!("{} exists", quote(&String::from_utf8_lossy(k)));
            failed = true;
        }
        Err(e) => {
            eprintln!("dbm/store: bad store: {e}");
            failed = true;
        }
    };
    if let Some((k, v)) = pair {
        put(k.as_bytes(), v.as_bytes());
    } else {
        for line in std::io::stdin().lock().split(b'\n') {
            let line = line.map_err(Error::from)?;
            let Some(at) = line.iter().position(|&b| b == b' ' || b == b'\t') else {
                eprintln!("dbm/store: bad input");
                purgatorio::exits("fail:bad input");
            };
            put(&line[..at], &line[at + 1..]);
        }
    }
    if failed {
        purgatorio::exits("fail:store");
    }
    Ok(())
}

/// `s` quoted as Inferno's `%q` does, when it needs it.
fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}
