//! whois [-h host] name ...: ask a whois server (RFC 3912; default
//! `whois.iana.org`, port 43) about each name and print its answer. `host`
//! may be any dial address; a bare name uses `tcp` and the `whois` port.

use std::fs::File;
use std::io::{Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, dial};

const USAGE: &str = "whois [-h host] name ...";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut host = String::from("whois.iana.org");
    while let Some(c) = arg.opt() {
        match c {
            'h' => host = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let names = arg.argv();
    if names.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let addr = match host.split('!').count() {
        1 => format!("tcp!{host}!43"),
        _ => host.clone(),
    };
    let mut out = std::io::stdout().lock();
    for name in &names {
        let conn = dial::dial(&addr).map_err(|e| Error(format!("can't dial {host}: {e}")))?;
        let data = conn.data.ok_or_else(|| Error::from("no connection"))?;
        let mut f = File::from(data);
        f.write_all(format!("{name}\r\n").as_bytes())?;
        let mut reply = Vec::new();
        f.read_to_end(&mut reply)?;
        let text = String::from_utf8_lossy(&reply).replace("\r\n", "\n");
        out.write_all(text.as_bytes())?;
    }
    Ok(())
}
