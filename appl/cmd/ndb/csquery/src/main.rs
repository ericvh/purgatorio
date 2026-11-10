//! ndb/csquery [-x net] [-s server] [address ...]: write each address to
//! the connection server (default `/net/cs`) and print every reply, one per
//! line. With no addresses, it prompts with `> ` and reads one address per
//! line from standard input. A query that fails prints `csquery: address:
//! error` and the exit status says so.

use std::fs::OpenOptions;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "csquery [-x net] [-s server] [address ...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut net = String::from("/net");
    let mut server = None;
    while let Some(c) = arg.opt() {
        match c {
            'x' => net = arg.earg()?,
            's' => server = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let server = server.unwrap_or_else(|| format!("{net}/cs"));
    let addrs = arg.argv();
    let mut ok = true;
    if addrs.is_empty() {
        let stdin = std::io::stdin();
        loop {
            print!("> ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                println!();
                break;
            }
            let addr = line.trim();
            if !addr.is_empty() {
                ok &= query(&server, addr);
            }
        }
    } else {
        for a in &addrs {
            ok &= query(&server, a);
        }
    }
    if !ok {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}

/// Prints the replies for `addr`, or the error.
fn query(server: &str, addr: &str) -> bool {
    match ask(server, addr) {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
            true
        }
        Err(e) => {
            eprintln!("csquery: {addr}: {e}");
            false
        }
    }
}

fn ask(server: &str, addr: &str) -> Result<Vec<String>, Error> {
    let err = |_| Error::last();
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(server)
        .map_err(err)?;
    f.write_all(addr.as_bytes()).map_err(err)?;
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    // Each read is one reply.
    let mut out = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = f.read(&mut buf).map_err(err)?;
        if n == 0 {
            return Ok(out);
        }
        let text = String::from_utf8_lossy(&buf[..n]);
        out.push(text.trim_end_matches('\n').to_string());
    }
}
