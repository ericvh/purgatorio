//! ndb/query [-a] [-f dbfile] attr [value [rattr]]: search an attribute
//! database (attrdb(6) format, default `/lib/ndb/local`).
//!
//! With `attr` alone, entries that have the attribute match; with a value,
//! entries with that pair. Without `rattr` a matching entry is printed on one
//! line as `attr=value` pairs. With `rattr` the values of that attribute are
//! printed one per line, taken from the line of the match when it has one and
//! otherwise from the whole entry. Without `-a` only the first match is
//! printed. A database that names other files with `database=` / `file=`
//! is searched through those files in order. No match prints nothing.

use ndb::{Entry, quote};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "ndb/query [-a] [-f dbfile] attr [value [rattr]]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut all = false;
    let mut file = String::from("/lib/ndb/local");
    while let Some(c) = arg.opt() {
        match c {
            'a' => all = true,
            'f' => file = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.is_empty() || rest.len() > 3 {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let attr = &rest[0];
    let value = rest.get(1);
    let rattr = rest.get(2);
    let db = match open(&file) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("ndb/query: {e}");
            purgatorio::exits("fail:error");
        }
    };
    let mut out = String::new();
    for e in &db {
        if !e.has(attr, value.map(String::as_str)) {
            continue;
        }
        match rattr {
            None => {
                out.push_str(&e.line());
                out.push('\n');
            }
            Some(r) => {
                let vals = e.near(attr, value.map(String::as_str), r);
                if vals.is_empty() {
                    continue;
                }
                for v in &vals {
                    out.push_str(&quote(v));
                    out.push('\n');
                    if !all {
                        break;
                    }
                }
            }
        }
        if !all {
            break;
        }
    }
    print!("{out}");
    Ok(())
}

/// Reads `file`, following a `database` entry's `file=` list.
fn open(file: &str) -> Result<Vec<Entry>, Error> {
    ndb::open(file, &|f| ndb::read_std(f, &purgatorio::errstr)).map_err(Error)
}
