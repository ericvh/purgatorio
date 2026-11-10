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

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "query [-a] [-f dbfile] attr [value [rattr]]";

/// One `attr=value` pair.
struct Pair {
    attr: String,
    val: String,
}

/// An entry: its lines, each a list of pairs.
type Entry = Vec<Vec<Pair>>;

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
            eprintln!("query: {e}");
            purgatorio::exits("fail:error");
        }
    };
    let mut out = String::new();
    for e in &db {
        // The line holding the match.
        let Some(line) = e.iter().find(|l| {
            l.iter()
                .any(|p| p.attr == *attr && value.is_none_or(|v| p.val == *v))
        }) else {
            continue;
        };
        match rattr {
            None => {
                let pairs: Vec<String> = e
                    .iter()
                    .flatten()
                    .map(|p| format!("{}={}", quote(&p.attr), quote(&p.val)))
                    .collect();
                out.push_str(&pairs.join(" "));
                out.push('\n');
            }
            Some(r) => {
                let near: Vec<&Pair> = line.iter().filter(|p| p.attr == *r).collect();
                let vals = if near.is_empty() {
                    e.iter().flatten().filter(|p| p.attr == *r).collect()
                } else {
                    near
                };
                if vals.is_empty() {
                    continue;
                }
                for p in &vals {
                    out.push_str(&quote(&p.val));
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
    let first = read(file)?;
    let files: Vec<String> = first
        .iter()
        .find(|e| e.iter().flatten().any(|p| p.attr == "database"))
        .map(|e| {
            e.iter()
                .flatten()
                .filter(|p| p.attr == "file")
                .map(|p| p.val.clone())
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return Ok(first);
    }
    let mut db = Vec::new();
    for f in &files {
        db.extend(read(f)?);
    }
    Ok(db)
}

fn read(file: &str) -> Result<Vec<Entry>, Error> {
    let text = std::fs::read_to_string(file)
        .map_err(|_| Error(format!("can't open {file}: {}", purgatorio::errstr())))?;
    let mut db: Vec<Entry> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let cont = raw.starts_with([' ', '\t']);
        let pairs = parse(raw).map_err(|e| Error(format!("{file}:{}: {e}", n + 1)))?;
        if pairs.is_empty() {
            continue;
        }
        match db.last_mut() {
            Some(e) if cont => e.push(pairs),
            _ => db.push(vec![pairs]),
        }
    }
    Ok(db)
}

/// Splits one line into pairs, stopping at `#`.
fn parse(line: &str) -> Result<Vec<Pair>, &'static str> {
    let mut out = Vec::new();
    let mut c = line.chars().peekable();
    loop {
        while c.next_if(|ch| ch.is_whitespace()).is_some() {}
        match c.peek() {
            None | Some('#') => return Ok(out),
            _ => {}
        }
        let attr = word(&mut c)?;
        let val = if c.next_if_eq(&'=').is_some() {
            word(&mut c)?
        } else {
            String::new()
        };
        out.push(Pair { attr, val });
    }
}

/// A plain or quoted word, ending at white space, `=` or `#`.
fn word(c: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String, &'static str> {
    let mut s = String::new();
    while let Some(&ch) = c.peek() {
        match ch {
            '\'' | '"' => {
                c.next();
                loop {
                    match c.next() {
                        None => return Err("missing quote"),
                        Some(q) if q == ch => {
                            if c.next_if_eq(&ch).is_some() {
                                s.push(ch);
                            } else {
                                break;
                            }
                        }
                        Some(o) => s.push(o),
                    }
                }
            }
            '=' | '#' => break,
            _ if ch.is_whitespace() => break,
            _ => {
                s.push(ch);
                c.next();
            }
        }
    }
    Ok(s)
}

/// Inferno's `%q`: quoted when empty or holding white space or a quote.
fn quote(s: &str) -> String {
    if !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}
