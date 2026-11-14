//! install/proto2list protofile ...: list the files the prototypes select
//! from `/`, sorted, one per line: the file, its new name (`-` when it is
//! the same), length, mode, modification time and CRC-32, separated by
//! tabs, and a final 0.

use std::io::Read;

use proto::{Level, Namespace};
use purgatorio::{Context, DMDIR, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        return Err(Error::from("usage: install/proto2list protofile ..."));
    }
    let mut files: Vec<(String, String)> = Vec::new();
    for p in &args[1..] {
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("fatal: can't open {p}: {}: skipping", Error::from(e));
                continue;
            }
        };
        let mut failed = false;
        proto::read(
            &text,
            "/",
            &Namespace,
            &mut |level, line, msg| {
                eprintln!("{p}: line {line} : {msg}");
                failed |= level == Level::Error;
            },
            &mut |f| {
                let new = if f.new == f.old {
                    String::from("-")
                } else {
                    f.new.clone()
                };
                files.push((f.old.clone(), new));
            },
        );
        if failed {
            purgatorio::exits("");
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.dedup_by(|b, a| {
        if a.0 != b.0 {
            return false;
        }
        if a.1 != b.1 {
            eprintln!(
                "{}: duplicate {}({} {})",
                args[args.len() - 1],
                a.0,
                a.1,
                b.1
            );
        }
        true
    });
    for (f, g) in files {
        let d = match purgatorio::stat(&f) {
            Ok(d) => d,
            Err(_) => {
                eprintln!("cannot open {f}");
                continue;
            }
        };
        let isdir = d.mode & DMDIR != 0;
        let length = if isdir { 0 } else { d.length };
        let crc = if isdir { 0 } else { crc(&f) };
        println!(
            "{f}\t{g}\t{length}\t{}\t{}\t{}\t0",
            d.mode as i32, d.mtime as i32, crc as i32
        );
    }
    Ok(())
}

fn crc(f: &str) -> u32 {
    let mut data = Vec::new();
    match std::fs::File::open(f).and_then(|mut h| h.read_to_end(&mut data)) {
        Ok(_) => proto::crc32(&data),
        Err(e) => {
            eprintln!("bad read on {f} : {}", Error::from(e));
            0
        }
    }
}
