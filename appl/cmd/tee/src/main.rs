//! tee [-a] file...: copy stdin to stdout and to each file.

use std::io::{Read, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut append = false;
    let mut files = Vec::new();
    for a in args.iter().skip(1) {
        if a == "-a" {
            append = true;
        } else {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(a)
                .unwrap_or_else(|_| {
                    eprintln!("tee: cannot open {a}: {}", purgatorio::errstr());
                    purgatorio::exits("fail:error")
                });
            files.push(f);
        }
    }
    let mut buf = [0u8; 8192];
    let mut input = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        for f in &mut files {
            f.write_all(&buf[..n])?;
        }
    }
    Ok(())
}
