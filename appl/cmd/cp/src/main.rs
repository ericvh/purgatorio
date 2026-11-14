//! cp [-r] file ... target: copy files. With one file, `target` may name
//! the new file or an existing directory; with several it must be a
//! directory. `-r` copies directories and their contents.

use std::fs::File;
use std::io::{self, Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error, abi::mode};

fn base(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

fn copy_file(from: &str, to: &str, perm: u32) -> Result<(), Error> {
    let mut src = File::open(from).map_err(|e| Error(format!("{from}: {}", Error::from(e))))?;
    let fd = purgatorio::create(to, mode::OWRITE, perm & 0o777)
        .map_err(|e| Error(format!("{to}: {e}")))?;
    let mut dst = File::from(fd);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        dst.write_all(&buf[..n])
            .map_err(|e: io::Error| Error(format!("{to}: {}", Error::from(e))))?;
    }
}

fn copy(from: &str, to: &str, recursive: bool) -> Result<(), Error> {
    let d = purgatorio::stat(from).map_err(|e| Error(format!("{from}: {e}")))?;
    if !d.qid.is_dir() {
        return copy_file(from, to, d.mode);
    }
    if !recursive {
        return Err(Error(format!("{from}: is a directory")));
    }
    if purgatorio::stat(to).is_err() {
        purgatorio::create(to, mode::OREAD, DMDIR | (d.mode & 0o777))
            .map_err(|e| Error(format!("{to}: {e}")))?;
    }
    for e in std::fs::read_dir(from)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        copy(&format!("{from}/{name}"), &format!("{to}/{name}"), true)?;
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "cp [-r] file ... target");
    let mut recursive = false;
    while let Some(c) = arg.opt() {
        match c {
            'r' => recursive = true,
            _ => return Err(arg.usage()),
        }
    }
    let mut files = arg.argv();
    if files.len() < 2 {
        return Err(Error::from("usage: cp [-r] file ... target"));
    }
    let target = files.pop().expect("at least two");
    let into_dir = purgatorio::stat(&target).is_ok_and(|d| d.qid.is_dir());
    if files.len() > 1 && !into_dir {
        return Err(Error(format!("{target}: not a directory")));
    }
    let mut failed = false;
    for f in &files {
        let to = if into_dir {
            format!("{}/{}", target.trim_end_matches('/'), base(f))
        } else {
            target.clone()
        };
        if let Err(e) = copy(f, &to, recursive) {
            eprintln!("cp: {e}");
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:error");
    }
    Ok(())
}
