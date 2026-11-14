//! chgrp [-uo] id file ...: change the group of each file to `id`, or with
//! `-u` or `-o` its owner, if the file server allows it.

use purgatorio::arg::Arg;
use purgatorio::{Context, Dir, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "chgrp [-uo] id file ...");
    let mut owner = false;
    while let Some(o) = arg.opt() {
        match o {
            'u' | 'o' => owner = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let words = arg.argv();
    let [id, files @ ..] = words.as_slice() else {
        return Err(usage);
    };
    if files.is_empty() {
        return Err(usage);
    }
    let mut failed = false;
    for f in files {
        let mut d = Dir::null();
        if owner {
            d.uid = id.clone();
        } else {
            d.gid = id.clone();
        }
        if let Err(e) = purgatorio::wstat(f, &d) {
            eprintln!("chgrp: can't change {f}: {e}");
            failed = true;
        }
    }
    if failed {
        purgatorio::exits("fail:error");
    }
    Ok(())
}
