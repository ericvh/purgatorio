//! install/mergelog [-dv] oldlog < newlog: combine a replica log and a
//! newer section of it into one, as Inferno's mergelog: the entries of
//! `oldlog`, then those on standard input, are replayed, and each path's
//! resulting entry is printed, in the order of the last change to each,
//! in the log format applylog(8) reads.
//!
//! A path's entry takes the newest metadata and time; an `m` keeps the
//! verb it changes (`a` then `m` is an `a`), any other verb replaces it,
//! and a deletion stays as `d`. Entries that make no sense in order (an
//! `a` for a path already there, a `c`, `m` or `d` for one that is not,
//! or a `d` of a file newer than the deletion) are skipped with a note
//! on standard error. `-v` also notes each entry as it is replayed; `-d`
//! is accepted for compatibility.
//!
//! Inferno's mergelog wanted three arguments yet refused a second one,
//! so it never ran; this one takes the old log alone. It prints entries
//! in the log's own format rather than Inferno's debugging one.

use std::collections::HashMap;
use std::io::Read;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use wrap::Entry;

const USAGE: &str = "install/mergelog [-dv] oldlog <newlog";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut verbose = false;
    while let Some(c) = arg.opt() {
        match c {
            'd' => {}
            'v' => verbose = true,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    if rest.len() > 1 {
        fail("restriction by path not yet done");
    }
    let old = match std::fs::read_to_string(&rest[0]) {
        Ok(t) => t,
        Err(e) => fail(&format!("can't open {}: {}", rest[0], Error::from(e))),
    };
    let mut new = String::new();
    std::io::stdin().read_to_string(&mut new)?;
    let mut entries = Vec::new();
    for line in old.lines().chain(new.lines()) {
        if line.trim().is_empty() {
            continue;
        }
        match Entry::parse(line) {
            Ok(e) => entries.push(e),
            Err(e) => fail(&e),
        }
    }
    for e in merge(&entries, verbose) {
        println!("{}", e.line());
    }
    Ok(())
}

fn fail(msg: &str) -> ! {
    eprintln!("mergelog: {msg}");
    purgatorio::exits("fail:error");
}

fn note(msg: &str) {
    eprintln!("mergelog: note: {msg}");
}

/// Replays `log`: each path's entry after it, ordered by the time and
/// sequence of its last change.
fn merge(log: &[Entry], verbose: bool) -> Vec<Entry> {
    let mut db: HashMap<String, Entry> = HashMap::new();
    for l in log {
        if verbose {
            note(&format!("replay {}", l.line()));
        }
        let cur = db.get(&l.path);
        let indb = cur.is_some_and(|e| e.verb != 'd');
        let q = wrap::quote(&l.path);
        let skip = match l.verb {
            'a' if indb => Some(format!("{q} duplicate create")),
            'c' if !indb => Some(format!("{q} contents but no entry")),
            'm' if !indb => Some(format!("{q} metadata but no entry")),
            'd' if !indb => Some(format!("{q} deleted but no entry")),
            'd' if cur.is_some_and(|e| e.mtime > l.mtime) => {
                Some(format!("{q} deleted but it's newer"))
            }
            _ => None,
        };
        if let Some(s) = skip {
            note(&s);
            continue;
        }
        let next = match db.get(&l.path) {
            Some(e) if indb => {
                let mut n = e.clone();
                if l.verb != 'd' {
                    n.mode = l.mode;
                    n.uid = l.uid.clone();
                    n.gid = l.gid.clone();
                    n.mtime = l.mtime;
                    n.length = l.length;
                    if l.digest.is_some() {
                        n.digest = l.digest.clone();
                    }
                }
                if l.verb != 'm' {
                    n.verb = l.verb;
                }
                n.serverpath = l.serverpath.clone();
                if (l.time, l.seq) > (n.time, n.seq) {
                    n.time = l.time;
                    n.seq = l.seq;
                }
                n
            }
            _ => l.clone(),
        };
        db.insert(l.path.clone(), next);
    }
    let mut out: Vec<Entry> = db.into_values().collect();
    out.sort_by(|a, b| (a.time, a.seq, &a.path).cmp(&(b.time, b.seq, &b.path)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str) -> Entry {
        Entry::parse(s).unwrap()
    }

    #[test]
    fn replay() {
        let log = [
            e("100 0 a a - 664 u g 10 1"),
            e("100 1 a b - 664 u g 10 2"),
            e("100 2 a c - 664 u g 10 2"),
            e("200 0 m a - 600 u g 10 1"),
            e("200 1 c b - 664 u g 20 5"),
            e("200 2 d c - 664 u g 30 0"),
            e("200 3 a b - 664 u g 30 0"),
        ];
        let got: Vec<String> = merge(&log, false).iter().map(Entry::line).collect();
        assert_eq!(
            got,
            [
                "200 0 a a - 600 u g 10 1",
                "200 1 c b - 664 u g 20 5",
                "200 2 d c - 664 u g 10 2",
            ]
        );
    }
}
