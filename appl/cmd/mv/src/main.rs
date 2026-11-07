//! mv file ... target: move files. Within one directory this renames
//! with a wstat of the name; across directories the file is copied and the
//! original removed. With several files, `target` must be a directory.

use std::fs::File;
use std::io::{Read, Write};

use purgatorio::{Context, Dir, Error, abi::mode};

fn split(path: &str) -> (&str, &str) {
    let p = path.trim_end_matches('/');
    match p.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((dir, name)) => (dir, name),
        None => (".", p),
    }
}

fn same_dir(a: &str, b: &str) -> bool {
    let canon = |d: &str| {
        if d.starts_with('/') || d.starts_with('#') {
            String::from(d)
        } else {
            let dot = purgatorio::dot().unwrap_or_default();
            if d == "." {
                dot
            } else {
                format!("{}/{d}", dot.trim_end_matches('/'))
            }
        }
    };
    canon(a) == canon(b)
}

fn copy_and_remove(from: &str, to: &str, d: &Dir) -> Result<(), Error> {
    if d.qid.is_dir() {
        return Err(Error(format!(
            "{from}: cannot move a directory to another directory"
        )));
    }
    let mut src = File::open(from)?;
    let mut dst = File::from(purgatorio::create(to, mode::OWRITE, d.mode & 0o777)?);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
    }
    purgatorio::remove(from)
}

fn mv(from: &str, to: &str) -> Result<(), Error> {
    let d = purgatorio::stat(from)?;
    let (fdir, _) = split(from);
    let (tdir, tname) = split(to);
    if same_dir(fdir, tdir) {
        if purgatorio::stat(to).is_ok() {
            purgatorio::remove(to)?;
        }
        let mut w = Dir::null();
        w.name = String::from(tname);
        return purgatorio::wstat(from, &w);
    }
    copy_and_remove(from, to, &d)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut files = args[1..].to_vec();
    if files.len() < 2 {
        return Err(Error::from("usage: mv file ... target"));
    }
    let target = files.pop().expect("at least two");
    let into_dir = purgatorio::stat(&target).is_ok_and(|d| d.qid.is_dir());
    if files.len() > 1 && !into_dir {
        return Err(Error(format!("{target}: not a directory")));
    }
    let mut failed = false;
    for f in &files {
        let to = if into_dir {
            format!("{}/{}", target.trim_end_matches('/'), split(f).1)
        } else {
            target.clone()
        };
        if let Err(e) = mv(f, &to) {
            eprintln!("mv: {f}: {e}");
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
