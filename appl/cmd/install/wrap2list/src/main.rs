//! install/wrap2list [-i] [-r root] [file ...]: list the files the
//! packages installed in `root/wrap` (default root `/`) hold, and the
//! files named, sorted, one per line in install/proto2list's format: the
//! file, `-`, its length, mode, modification time (0 with `-i`) and
//! CRC-32, separated by tabs, and a final 0. A file an update removed is
//! left out, and so is one that is not there. `-r` is ours; Inferno's
//! reads `/wrap` only.

use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};

const USAGE: &str = "install/wrap2list [-i] [-r root] [file ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut init, mut root) = (false, String::from("/"));
    while let Some(c) = arg.opt() {
        match c {
            'i' => init = true,
            'r' => root = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let mut names: Vec<String> = arg.argv();
    let wraps = proto::mkpath(&root, "wrap");
    if let Ok(rd) = std::fs::read_dir(&wraps) {
        let mut pkgs: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        pkgs.sort();
        for p in pkgs {
            let units = wrap::units(&format!("{wraps}/{p}"));
            let mut set: std::collections::BTreeSet<String> = Default::default();
            for u in &units {
                for r in &u.remove {
                    set.remove(r);
                }
                set.extend(u.md5.iter().map(|(n, _)| n.clone()));
            }
            names.extend(set);
        }
    }
    names.sort();
    names.dedup();
    for f in names {
        let path = proto::mkpath(&root, &f);
        let Ok(d) = purgatorio::stat(&path) else {
            continue;
        };
        let isdir = d.mode & DMDIR != 0;
        let length = if isdir { 0 } else { d.length };
        let crc = if isdir {
            0
        } else {
            std::fs::read(&path).map_or(0, |b| proto::crc32(&b))
        };
        let mtime = if init { 0 } else { d.mtime };
        println!(
            "{f}\t-\t{length}\t{}\t{}\t{}\t0",
            d.mode as i32, mtime as i32, crc as i32
        );
    }
    Ok(())
}
