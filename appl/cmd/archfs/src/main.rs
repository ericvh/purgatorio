//! archfs [-ab] [-s] [-m mntpt] archive [prefix ...]: mount a read-only
//! view of an archive in the format `mkfs -a` writes on `mntpt` (default
//! `/mnt/arch`). Each member is a header line, `name mode uid gid mtime
//! length` with the mode in octal, followed by `length` bytes of data; the
//! line `end of archive` ends it. With prefixes, only members whose names
//! start with one of them, as whole path elements, are served; `-s` stops
//! reading at the first member after a match that matches none.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use purgatorio::abi::mount::MREPL;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};
use styxserv::rotree::RoTree;

const USAGE: &str = "archfs [-ab] [-s] [-m mntpt] archive [prefix ...]";

/// Whether `name` is `prefix` or lies under it.
fn under(name: &str, prefix: &str) -> bool {
    let p = prefix.trim_matches('/');
    p.is_empty()
        || name == p
        || (name.starts_with(p) && name.as_bytes().get(p.len()) == Some(&b'/'))
}

fn load(path: &str, prefixes: &[String], stop: bool) -> Result<RoTree, String> {
    let f = File::open(path).map_err(|e| format!("cannot open {path}: {}", styxserv::ioerr(e)))?;
    let mut r = BufReader::new(f);
    let mut t = RoTree::new(Dir {
        mode: DMDIR | 0o555,
        uid: String::from("inferno"),
        gid: String::from("inferno"),
        muid: String::from("inferno"),
        ..Dir::default()
    });
    let mut matched = false;
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line).map_err(styxserv::ioerr)?;
        if n == 0 || line.last() != Some(&b'\n') {
            return Err(String::from("premature end of archive"));
        }
        let line = String::from_utf8_lossy(&line);
        if line == "end of archive\n" {
            return Ok(t);
        }
        let f = purgatorio::unquote_fields(&line);
        let [name, mode, uid, gid, mtime, length] = f.as_slice() else {
            return Err(String::from("too few fields in file header"));
        };
        let bad = || String::from("bad file header");
        let mode = u32::from_str_radix(mode, 8).map_err(|_| bad())?;
        let mtime: u32 = mtime.parse().map_err(|_| bad())?;
        let length: u64 = length.parse().map_err(|_| bad())?;
        let mut data = Vec::new();
        (&mut r)
            .take(length)
            .read_to_end(&mut data)
            .map_err(styxserv::ioerr)?;
        if (data.len() as u64) < length {
            return Err(String::from("premature end of archive"));
        }
        let name = name.trim_start_matches('/');
        let wanted = prefixes.is_empty() || prefixes.iter().any(|p| under(name, p));
        if !wanted {
            if stop && matched {
                return Ok(t);
            }
            continue;
        }
        matched = true;
        let d = Dir {
            mode,
            atime: mtime,
            mtime,
            uid: uid.clone(),
            gid: gid.clone(),
            muid: uid.clone(),
            ..Dir::default()
        };
        if mode & DMDIR != 0 {
            data.clear();
        }
        t.add(name, d, data);
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let stop = args.get(2).map(String::as_str) == Some("-s");
        let rest = &args[2 + usize::from(stop)..];
        let (path, prefixes) = rest.split_first().ok_or_else(|| Error::from(USAGE))?;
        let mut t = load(path, prefixes, stop)?;
        return styxserv::bg::serve(&mut t);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut flags = MREPL;
    let mut stop = false;
    let mut mtpt = String::from("/mnt/arch");
    while let Some(c) = arg.opt() {
        match c {
            'a' | 'b' => {
                styxserv::bg::flag(c, &mut flags);
            }
            's' => stop = true,
            'm' => mtpt = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let Some((file, prefixes)) = rest.split_first() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let file = styxserv::abs(file);
    load(&file, prefixes, stop)?;
    let mut argv = vec![String::from("archfs"), String::from("--serve")];
    if stop {
        argv.push(String::from("-s"));
    }
    argv.push(file);
    argv.extend(prefixes.iter().cloned());
    styxserv::bg::mount("/bin/archfs.wasm", &argv, &mtpt, flags)
        .map_err(|e| Error(format!("cannot mount archive on {mtpt}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn prefixes() {
        assert!(under("lib/x", "lib"));
        assert!(under("lib", "/lib/"));
        assert!(!under("libx/y", "lib"));
        assert!(under("a/b/c", "a/b"));
    }
}
