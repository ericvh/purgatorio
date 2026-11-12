//! ndb/dnsquery [-x net] [name ...]: ask the DNS resolver (`net/dns`,
//! started by `ndb/dns`) about each name, with the default type (`ip`, or
//! `ptr` for an address), and print every answer, one per line. With no
//! names, it prompts with `> ` and reads `name [type]` queries from
//! standard input, one per line. A query that fails prints `dnsquery:
//! query: error` and the exit status says so.

use std::fs::OpenOptions;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "ndb/dnsquery [-x net] [name ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut net = String::from("/net");
    while let Some(c) = arg.opt() {
        match c {
            'x' => net = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let server = format!("{net}/dns");
    let names = arg.argv();
    let mut ok = true;
    if names.is_empty() {
        let stdin = std::io::stdin();
        loop {
            print!("> ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                println!();
                break;
            }
            let q = line.trim();
            if !q.is_empty() {
                ok &= query(&server, q);
            }
        }
    } else {
        for n in &names {
            ok &= query(&server, n);
        }
    }
    if !ok {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}

fn query(server: &str, q: &str) -> bool {
    match ask(server, q) {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
            true
        }
        Err(e) => {
            eprintln!("ndb/dnsquery: {q}: {e}");
            false
        }
    }
}

fn ask(server: &str, q: &str) -> Result<Vec<String>, Error> {
    let err = |_| Error::last();
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(server)
        .map_err(err)?;
    f.write_all(q.as_bytes()).map_err(err)?;
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = f.read(&mut buf).map_err(err)?;
        if n == 0 {
            return Ok(out);
        }
        out.push(
            String::from_utf8_lossy(&buf[..n])
                .trim_end_matches('\n')
                .to_string(),
        );
    }
}
