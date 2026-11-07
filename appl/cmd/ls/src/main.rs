//! ls [-dlp] [name ...]: list files, and the contents of directories, in
//! name order. `-d` lists directories themselves, `-l` gives the long
//! form (mode, device, uid, gid, length, modification time, name) and `-p`
//! prints only the last element of each name. Names in a union directory
//! that appear more than once are listed once, as a walk would find them.

use std::io::Write;

use purgatorio::arg::Arg;
use purgatorio::{Context, DMAPPEND, DMAUTH, DMDIR, DMEXCL, Dir, Error, abi::mode, daytime};

struct Opts {
    dirs: bool,
    long: bool,
    last: bool,
}

/// Plan 9's mode string: `d` (or `a`, `A`) or `-`, then `l` or `-`, then
/// rwx for user, group and other.
fn mode_string(m: u32) -> String {
    let mut s = String::new();
    s.push(if m & DMDIR != 0 {
        'd'
    } else if m & DMAPPEND != 0 {
        'a'
    } else if m & DMAUTH != 0 {
        'A'
    } else {
        '-'
    });
    s.push(if m & DMEXCL != 0 { 'l' } else { '-' });
    for shift in [6, 3, 0] {
        let b = m >> shift;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn read_dir(path: &str) -> Result<Vec<Dir>, Error> {
    let fd = purgatorio::open(path, mode::OREAD)?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&fd);
    let mut all: Vec<Dir> = Vec::new();
    loop {
        let batch = purgatorio::dirread(fd)?;
        if batch.is_empty() {
            break;
        }
        for d in batch {
            if !all.iter().any(|e| e.name == d.name) {
                all.push(d);
            }
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

fn join(dir: &str, name: &str) -> String {
    match dir {
        "." => String::from(name),
        d if d.ends_with('/') => format!("{d}{name}"),
        d => format!("{d}/{name}"),
    }
}

fn print(out: &mut Vec<(String, Dir)>, o: &Opts) -> String {
    let now = daytime::now();
    let w = |f: fn(&Dir) -> usize| out.iter().map(|(_, d)| f(d)).max().unwrap_or(0);
    let (wu, wg, wl) = (
        w(|d| d.uid.len()),
        w(|d| d.gid.len()),
        w(|d| d.length.to_string().len()),
    );
    let mut s = String::new();
    for (name, d) in out.drain(..) {
        let name = if o.last {
            String::from(name.rsplit('/').find(|e| !e.is_empty()).unwrap_or(&name))
        } else {
            name
        };
        if o.long {
            let dc = char::from_u32(u32::from(d.dtype)).unwrap_or('?');
            s.push_str(&format!(
                "{} {} {} {:wu$} {:wg$} {:>wl$} {} {}\n",
                mode_string(d.mode),
                dc,
                d.dev,
                d.uid,
                d.gid,
                d.length,
                daytime::filet(now, i64::from(d.mtime)),
                name
            ));
        } else {
            s.push_str(&name);
            s.push('\n');
        }
    }
    s
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "ls [-dlp] [name ...]");
    let mut o = Opts {
        dirs: false,
        long: false,
        last: false,
    };
    while let Some(c) = arg.opt() {
        match c {
            'd' => o.dirs = true,
            'l' => o.long = true,
            'p' => o.last = true,
            _ => return Err(arg.usage()),
        }
    }
    let mut names = arg.argv();
    if names.is_empty() {
        names.push(String::from("."));
    }
    let mut failed = false;
    let mut rows = Vec::new();
    for name in &names {
        let res = purgatorio::stat(name).and_then(|d| {
            if d.qid.is_dir() && !o.dirs {
                for e in read_dir(name)? {
                    rows.push((join(name, &e.name), e));
                }
            } else {
                rows.push((name.clone(), d));
            }
            Ok(())
        });
        if let Err(e) = res {
            eprintln!("ls: {name}: {e}");
            failed = true;
        }
    }
    let text = print(&mut rows, &o);
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
