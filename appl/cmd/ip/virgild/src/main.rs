//! ip/virgild [-d] [-a addr]: answer virgil queries, as Inferno's
//! virgild(8). A client broadcasts `userid?machine` to UDP port 2202
//! (`virgil`); virgild asks `/net/cs` for `tcp!machine!1000` and sends
//! back `userid?machine=address`. A name cs cannot translate gets no
//! answer, so another server may give it.
//!
//! The port is announced in `headers` mode, so one conversation reads
//! every request and each answer goes back to its sender. `-a` announces
//! another address than `udp!*!2202` and `-d` says on standard output
//! which address was announced and traces each query; both are
//! Purgatorio's additions, for tests on loopback.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, dial};

const USAGE: &str = "ip/virgild [-d] [-a addr]";
/// The address header of a `headers` mode UDP conversation (ip(3)).
const UDP_HEADER: usize = 52;
/// The largest datagram read, header included.
const MAX: usize = 512;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut debug = false;
    let mut addr = String::from("udp!*!2202");
    while let Some(c) = arg.opt() {
        match c {
            'd' => debug = true,
            'a' => addr = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let (_ctl, mut data, local) = announce(&addr)?;
    if debug {
        println!("virgild: announced {local}");
    }
    let mut buf = vec![0u8; MAX];
    loop {
        let n = data.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        if n <= UDP_HEADER + 1 {
            continue;
        }
        let body = &buf[UDP_HEADER..n];
        let end = body
            .iter()
            .position(|&b| b == b' ' || b == 0 || b == b'\n')
            .unwrap_or(body.len());
        let request = String::from_utf8_lossy(&body[..end]).into_owned();
        let Some(answer) = query(&request) else {
            if debug {
                println!("virgild: {request}: no answer");
            }
            continue;
        };
        if debug {
            println!("virgild: {answer}");
        }
        let mut reply = buf[..UDP_HEADER].to_vec();
        reply.extend_from_slice(answer.as_bytes());
        if reply.len() <= MAX {
            let _ = data.write_all(&reply);
        }
    }
}

/// `userid?machine=address`, or `None` for a bad request or a name cs
/// does not know.
fn query(request: &str) -> Option<String> {
    let fields: Vec<&str> = request.split('?').filter(|f| !f.is_empty()).collect();
    if fields.len() < 2 {
        eprintln!("ip/virgild: bad request {request} {}", request.len());
        return None;
    }
    let machine = fields[1];
    let t = dial::translate(&format!("tcp!{machine}!1000")).ok()?;
    let host = t.first()?.split('!').nth(1)?.to_string();
    Some(format!("{request}={host}"))
}

/// Announces `addr` in `headers` mode: the ctl file (held open), the
/// data file and the announced address.
fn announce(addr: &str) -> Result<(File, File, String), Error> {
    let addr = dial::netmkaddr(addr);
    let parts: Vec<&str> = addr.splitn(3, '!').collect();
    let [net, host, port] = parts.as_slice() else {
        return Err(Error(format!("bad address {addr}")));
    };
    let port = match port.parse::<u16>() {
        Ok(p) => p.to_string(),
        Err(_) => port_number(port)?,
    };
    let mut ctl = rw(&format!("/net/{net}/clone"))?;
    let mut idbuf = [0u8; 32];
    let k = ctl.read(&mut idbuf)?;
    let id = String::from_utf8_lossy(&idbuf[..k]).trim().to_string();
    ctl.write_all(format!("announce {host}!{port}").as_bytes())
        .map_err(|e| Error(format!("can't open port: {}", Error::from(e).0)))?;
    ctl.write_all(b"headers")
        .map_err(|e| Error(format!("can't set headers: {}", Error::from(e).0)))?;
    let dir = format!("/net/{net}/{id}");
    let data = rw(&format!("{dir}/data")).map_err(|_| Error::from("can't open data file"))?;
    let local = dial::local(&dir)?;
    Ok((ctl, data, local))
}

/// A service's port number, from `/net/cs`.
fn port_number(service: &str) -> Result<String, Error> {
    let t = dial::translate(&format!("udp!*!{service}"))?;
    t.first()
        .and_then(|a| a.rsplit('!').next())
        .filter(|p| p.parse::<u16>().is_ok())
        .map(String::from)
        .ok_or_else(|| Error(format!("unknown service {service}")))
}

fn rw(path: &str) -> Result<File, Error> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error(format!("can't open {path}: {}", Error::from(e).0)))
}
