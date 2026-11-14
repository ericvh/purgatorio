//! install/applylog [-cegnsuv] [-T timefile] clientlog clientroot
//! serverroot [path ...]: bring the replica at `clientroot` up to date
//! with the log entries on standard input, copying files from
//! `serverroot`, as applylog(8). `clientlog` holds the entries already
//! applied: each one applied is added to it.
//!
//! Before it applies an entry, applylog checks that the replica's file is
//! as `clientlog` says it was left (same modification time and length, or
//! absent if it never had it). If not, the replica was changed on its own:
//! the conflict is reported and the entry not applied, unless `-s` (make
//! the replica match the server) or `-c` (keep the replica's file, and
//! count the entry as applied). `-n` prints what would be done and changes
//! nothing; `-v` prints each entry as it is examined; `-u` and `-g` also
//! set the owner and group from the entry. With `-T timefile`, entries no
//! later than the time and sequence number in `timefile` are skipped, and
//! the last one applied is written there. `-e` makes any error, conflicts
//! included, give an error status. With paths, only entries below them
//! are applied.

use std::io::{BufRead, Write};

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};
use wrap::{Entry, quote};

const USAGE: &str =
    "install/applylog [-cegnsuv] [-T timefile] clientlog clientroot serverroot [path ...]";

struct Apply {
    client: String,
    server: String,
    dry: bool,
    owners: bool,
    groups: bool,
}

impl Apply {
    fn wstat(&self, path: &str, e: &Entry, times: bool) -> Result<(), String> {
        let mut d = Dir::null();
        d.mode = e.mode;
        if times {
            d.mtime = e.mtime;
        }
        if self.groups {
            d.gid = e.gid.clone();
        }
        purgatorio::wstat(path, &d).map_err(|err| format!("{}: wstat: {err}", quote(path)))?;
        if self.owners {
            let mut d = Dir::null();
            d.uid = e.uid.clone();
            purgatorio::wstat(path, &d).map_err(|err| format!("{}: wstat: {err}", quote(path)))?;
        }
        Ok(())
    }

    fn apply(&self, e: &Entry) -> Result<(), String> {
        let path = proto::mkpath(&self.client, &e.path);
        match e.verb {
            'a' | 'c' if e.mode & DMDIR != 0 => {
                if purgatorio::stat(&path).is_err() {
                    purgatorio::create(&path, mode::OREAD, DMDIR | (e.mode & 0o777))
                        .map_err(|err| format!("can't create {}: {err}", quote(&path)))?;
                }
                self.wstat(&path, e, true)
            }
            'a' | 'c' => {
                let src = e.server(&self.server);
                let data = std::fs::read(&src)
                    .map_err(|err| format!("{}: {}", quote(&src), Error::from(err)))?;
                if purgatorio::stat(&path).is_ok() {
                    let _ = purgatorio::remove(&path);
                }
                let fd = purgatorio::create(&path, mode::OWRITE, (e.mode & 0o777) | 0o200)
                    .map_err(|err| format!("can't create {}: {err}", quote(&path)))?;
                std::fs::File::from(fd)
                    .write_all(&data)
                    .map_err(|err| format!("{}: write: {}", quote(&path), Error::from(err)))?;
                // The replica's time is the server's, so the next check
                // matches what the log says.
                let entry = Entry {
                    length: data.len() as u64,
                    ..e.clone()
                };
                self.wstat(&path, &entry, true)
            }
            'd' => match purgatorio::stat(&path) {
                Ok(_) => purgatorio::remove(&path)
                    .map_err(|err| format!("can't remove {}: {err}", quote(&path))),
                Err(_) => Ok(()),
            },
            _ => self.wstat(&path, e, false),
        }
    }

    /// Why the replica's file is not as the client log left it, if it is
    /// not.
    fn conflict(&self, e: &Entry, last: Option<&Entry>) -> Option<&'static str> {
        let path = proto::mkpath(&self.client, &e.path);
        let now = purgatorio::stat(&path).ok();
        match (last, now) {
            (None, Some(_)) if e.verb != 'd' => Some("created locally"),
            (Some(_), None) if e.verb != 'a' => Some("removed locally"),
            (Some(l), Some(d)) => {
                let isdir = d.mode & DMDIR != 0;
                if !isdir && (d.mtime != l.mtime || d.length != l.length) {
                    Some("changed locally")
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn read_stamp(path: &str) -> Result<(u32, u32), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let f: Vec<&str> = text.split_whitespace().collect();
    match f.as_slice() {
        [] => Ok((0, 0)),
        [t, g] => Ok((
            t.parse().map_err(|_| format!("{path}: bad time"))?,
            g.parse().map_err(|_| format!("{path}: bad time"))?,
        )),
        _ => Err(format!("{path}: bad time")),
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut client_wins, mut server_wins, mut strict, mut verbose) = (false, false, false, false);
    let (mut dry, mut owners, mut groups, mut timefile) = (false, false, false, None);
    while let Some(c) = arg.opt() {
        match c {
            'c' => client_wins = true,
            's' => server_wins = true,
            'e' => strict = true,
            'v' => verbose = true,
            'n' => dry = true,
            'u' => owners = true,
            'g' => groups = true,
            'T' => timefile = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [clientlog, client, server, paths @ ..] = rest.as_slice() else {
        return Err(Error::from(format!("usage: {USAGE}")));
    };
    let ap = Apply {
        client: client.clone(),
        server: server.clone(),
        dry,
        owners,
        groups,
    };
    let mut last: std::collections::HashMap<String, Entry> =
        wrap::state(&wrap::read_log(clientlog).map_err(Error)?)
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
    let since = match &timefile {
        Some(t) => Some(read_stamp(t).map_err(Error)?),
        None => None,
    };
    let mut logf = if dry {
        None
    } else {
        Some(
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(clientlog)
                .map_err(|e| Error(format!("{clientlog}: {}", Error::from(e))))?,
        )
    };
    let mut errors = 0;
    let mut stamp = None;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let e = Entry::parse(&line).map_err(Error)?;
        if since.is_some_and(|s| (e.time, e.seq) <= s) {
            continue;
        }
        let wanted = paths.is_empty()
            || paths.iter().any(|p| {
                let p = format!("/{}", p.trim_matches('/'));
                e.path == p || e.path.starts_with(&format!("{p}/")) || p == "/"
            });
        if !wanted {
            continue;
        }
        if verbose {
            println!("{}", e.line());
        }
        let mut applied = true;
        match ap.conflict(&e, last.get(&e.path)) {
            Some(why) if !server_wins => {
                if client_wins {
                    println!("{} {}: {why}; kept", e.verb, quote(&e.path));
                } else {
                    println!("conflict: {} {}: {why}", e.verb, quote(&e.path));
                    errors += 1;
                    continue;
                }
                applied = false;
            }
            _ => {}
        }
        if ap.dry {
            if applied {
                println!("{} {}", e.verb, quote(&e.path));
            }
            continue;
        }
        if applied && let Err(err) = ap.apply(&e) {
            eprintln!("install/applylog: {err}");
            errors += 1;
            continue;
        }
        if let Some(f) = &mut logf {
            writeln!(f, "{}", e.line())?;
        }
        // What the replica holds now, for the checks of later entries.
        let now = purgatorio::stat(&proto::mkpath(client, &e.path)).ok();
        match (e.verb, now) {
            ('d', _) => {
                last.remove(&e.path);
            }
            (_, Some(d)) => {
                last.insert(
                    e.path.clone(),
                    Entry {
                        mtime: d.mtime,
                        length: d.length,
                        ..e.clone()
                    },
                );
            }
            _ => {}
        }
        stamp = Some((e.time, e.seq));
    }
    if let (Some(t), Some((time, seq)), false) = (&timefile, stamp, dry) {
        std::fs::write(t, format!("{time} {seq}\n"))
            .map_err(|e| Error(format!("{t}: {}", Error::from(e))))?;
    }
    if strict && errors > 0 {
        purgatorio::exits(&format!("fail:{errors} errors"));
    }
    Ok(())
}
