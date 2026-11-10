//! touch [-c] [-t time] file ...: set each file's modification time to
//! `time` (seconds since the epoch) or now, creating files that do not
//! exist unless `-c` is given.

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, Dir, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "touch [-c] [-t time] file ...");
    let mut nocreate = false;
    let mut time: Option<u32> = None;
    while let Some(o) = arg.opt() {
        match o {
            'c' => nocreate = true,
            't' => {
                let t = arg.earg()?;
                time = Some(t.parse().map_err(|_| arg.usage())?);
            }
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let files = arg.argv();
    if files.is_empty() {
        return Err(usage);
    }
    let now = time.unwrap_or_else(|| purgatorio::daytime::now() as u32);
    let mut failed = false;
    for f in &files {
        let mut d = Dir::null();
        d.mtime = now;
        if purgatorio::wstat(f, &d).is_ok() {
            continue;
        }
        if purgatorio::stat(f).is_ok() || nocreate {
            eprintln!("touch: {f}: cannot change time: {}", purgatorio::errstr());
            failed = true;
            continue;
        }
        match purgatorio::create(f, mode::OREAD, 0o666) {
            Ok(fd) => {
                drop(fd);
                if time.is_some() && purgatorio::wstat(f, &d).is_err() {
                    eprintln!("touch: {f}: cannot change time: {}", purgatorio::errstr());
                    failed = true;
                }
            }
            Err(e) => {
                eprintln!("touch: {f}: cannot create: {e}");
                failed = true;
            }
        }
    }
    if failed {
        purgatorio::exits("fail:touch");
    }
    Ok(())
}
