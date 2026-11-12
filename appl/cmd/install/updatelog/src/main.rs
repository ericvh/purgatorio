//! install/updatelog [-c] [-p proto] [-r root] [-t now gen] [-x path]...
//! log [path ...]: print the log entries that bring the replica `log`
//! records up to date with the tree at `root` (default `.`), as
//! applylog(8): `a` for a file the log does not have, `c` for one whose
//! modification time or length changed, `m` for one whose mode, owner or
//! group did, and `d` for one that has gone (deepest first, after the
//! others). The files are those the prototype (default `/lib/proto/all`)
//! selects, limited to the paths named and without those under `-x`.
//! Each entry gets the time `now` (default the time) and the next
//! sequence number from `gen` (default 0). `-c` prints only changes, no
//! additions or deletions. The output is meant to be appended to the log.

use proto::{Level, Namespace};
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};
use wrap::Entry;

const USAGE: &str =
    "install/updatelog [-c] [-p proto] [-r root] [-t now gen] [-x path]... log [path ...]";

fn under(name: &str, p: &str) -> bool {
    let p = p.trim_end_matches('/');
    let p = if p.starts_with('/') {
        String::from(p)
    } else {
        format!("/{p}")
    };
    p == "/" || name == p || name.starts_with(&format!("{p}/"))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut changes_only = false;
    let (mut proto_file, mut root) = (String::from("/lib/proto/all"), String::from("."));
    let (mut now, mut seq) = (None, 0u32);
    let mut excluded = Vec::new();
    while let Some(c) = arg.opt() {
        match c {
            'c' => changes_only = true,
            'p' => proto_file = arg.earg()?,
            'r' => root = arg.earg()?,
            't' => {
                let (t, g) = (arg.earg()?, arg.earg()?);
                now = Some(t.parse::<u32>().map_err(|_| arg.usage())?);
                seq = g.parse().map_err(|_| arg.usage())?;
            }
            'x' => excluded.push(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let mut rest = arg.argv();
    if rest.is_empty() {
        return Err(Error::from(format!("usage: {USAGE}")));
    }
    let log = rest.remove(0);
    let paths = rest;
    let now = now.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32)
    });
    let in_scope = |name: &str| {
        (paths.is_empty() || paths.iter().any(|p| under(name, p)))
            && !excluded.iter().any(|x| under(name, x))
    };
    let entries = wrap::read_log(&log).map_err(Error)?;
    let mut state: std::collections::BTreeMap<String, Entry> = wrap::state(&entries)
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let text = std::fs::read_to_string(&proto_file)
        .map_err(|e| Error(format!("{proto_file}: {}", Error::from(e))))?;
    let mut files = Vec::new();
    let mut failed = false;
    proto::read(
        &text,
        &root,
        &Namespace,
        &mut |level, line, msg| {
            eprintln!("install/updatelog: {proto_file}:{line}: {msg}");
            failed |= level == Level::Error;
        },
        &mut |f| files.push(f.clone()),
    );
    if failed {
        purgatorio::exits("error");
    }
    let mut out = |verb: char, path: &str, d: &Entry| {
        let e = Entry {
            time: now,
            seq,
            verb,
            path: String::from(path),
            ..d.clone()
        };
        seq += 1;
        println!("{}", e.line());
    };
    for f in files {
        if f.new == "/" || !in_scope(&f.new) {
            continue;
        }
        let d = &f.dir;
        let isdir = d.mode & DMDIR != 0;
        let now_entry = Entry {
            time: now,
            seq: 0,
            verb: 'a',
            path: f.new.clone(),
            serverpath: None,
            mode: d.mode,
            uid: d.uid.clone(),
            gid: d.gid.clone(),
            mtime: d.mtime,
            length: if isdir { 0 } else { d.length },
            digest: None,
        };
        match state.remove(&f.new) {
            None => {
                if !changes_only {
                    out('a', &f.new, &now_entry);
                }
            }
            Some(old) => {
                if !isdir && (old.mtime != d.mtime || old.length != d.length) {
                    out('c', &f.new, &now_entry);
                } else if old.mode != d.mode || old.uid != d.uid || old.gid != d.gid {
                    out('m', &f.new, &now_entry);
                }
            }
        }
    }
    if !changes_only {
        // What is left has gone from the tree.
        let gone: Vec<Entry> = state.into_values().filter(|e| in_scope(&e.path)).collect();
        for e in gone.iter().rev() {
            out('d', &e.path, e);
        }
    }
    Ok(())
}
