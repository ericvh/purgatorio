//! ip/tftpd [-dr] [-p port] [-h homedir] [-x net]: the TFTP server
//! (RFC 1350). It announces `udp!*!port` (default 69) on `net` (default
//! `/net`) and serves each client's read or write request in its own prog.
//! `port` may also be `addr!port`, to announce on one address only: on the
//! probe and the kernel, loopback is a network of its own that `*` does not
//! cover, so a server for loopback clients announces `127.0.0.1!port`.
//!
//! Names are looked up in `homedir` (default `/services/tftpd`); with `-r`
//! absolute names are too, otherwise they are used as they are. A name
//! with `..` is refused. A file is sent only if everyone may read it, and
//! written only if it exists and everyone may write it. `-d` traces each
//! request on standard output. Both `octet` and `netascii` are sent as
//! they are stored.
//!
//! `#I` has no headers mode for announced UDP ports, so replies come from
//! the announced port rather than a fresh one per transfer; clients take
//! their transfer port from the first reply, so this works with them.

use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};

use purgatorio::abi::pctl;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error, dial};

const USAGE: &str = "ip/tftpd [-dr] [-p port] [-h homedir] [-x net]";
const SELF: &str = "/bin/ip/tftpd.wasm";
const XFER: &str = "--xfer";
const SEGSIZE: usize = 512;
const TRIES: usize = 5;
const WAIT_MS: u64 = 2000;

const RRQ: u16 = 1;
const WRQ: u16 = 2;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;

const ENOTFOUND: u16 = 1;
const EACCESS: u16 = 2;
const EBADOP: u16 = 4;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut argv = args;
    // `--xfer dir`: serve the conversation `dir`, whose data is fd 0.
    let xfer = if argv.get(1).map(String::as_str) == Some(XFER) && argv.len() > 2 {
        argv.remove(1);
        Some(argv.remove(1))
    } else {
        None
    };
    let mut arg = Arg::new(argv.clone(), USAGE);
    let mut debug = false;
    let mut restrict = false;
    let mut port = String::from("69");
    let mut home = String::from("/services/tftpd");
    let mut net = String::from("/net");
    while let Some(c) = arg.opt() {
        match c {
            'd' => debug = true,
            'r' => restrict = true,
            'p' => port = arg.earg()?,
            'h' => home = arg.earg()?,
            'x' => net = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let conf = Conf {
        debug,
        restrict,
        home,
    };
    if let Some(dir) = xfer {
        return transfer(&conf, &dir);
    }
    let port = match port.split_once('!') {
        Some(_) => port,
        None => format!("*!{port}"),
    };
    let addr = format!("udp!{port}");
    let ann = if net == "/net" {
        dial::announce(&addr)
    } else {
        announce_on(&net, &port)
    }
    .map_err(|e| Error(format!("can't announce {addr}: {e}")))?;
    if debug {
        println!(
            "tftpd: announced {}",
            dial::local(&ann.dir).unwrap_or_default()
        );
    }
    loop {
        let conn = match dial::listen(&ann.dir) {
            Ok(c) => c,
            // Killed: stop quietly.
            Err(e) if e.0 == purgatorio::abi::error::EINTR => return Ok(()),
            Err(e) => return Err(Error(format!("listen: {e}"))),
        };
        let Some(data) = conn.data.as_ref() else {
            continue;
        };
        let mut fds = vec![(data.as_raw_fd(), 0), (2, 2)];
        if debug {
            fds.push((1, 1));
        }
        let mut child = vec![argv[0].clone(), String::from(XFER), conn.dir.clone()];
        child.extend(argv.iter().skip(1).cloned());
        if let Err(e) = purgatorio::spawn(SELF, &child, pctl::NEWFD, &fds) {
            eprintln!("ip/tftpd: {e}");
        }
    }
}

/// `dial::announce` for a network mounted elsewhere than `/net`.
fn announce_on(net: &str, port: &str) -> Result<dial::Connection, Error> {
    use std::fs::OpenOptions;
    let mut ctl = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{net}/udp/clone"))?;
    let mut buf = [0u8; 32];
    let n = ctl.read(&mut buf)?;
    let id = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    ctl.write_all(format!("announce {port}").as_bytes())
        .map_err(|_| Error::last())?;
    Ok(dial::Connection {
        dir: format!("{net}/udp/{id}"),
        ctl: ctl.into(),
        data: None,
    })
}

struct Conf {
    debug: bool,
    restrict: bool,
    home: String,
}

/// `tftpd --xfer dir`: one client's request, on fd 0. The conversation
/// is hung up at the end, so the client's next request starts a new one.
fn transfer(conf: &Conf, dir: &str) -> Result<(), Error> {
    // SAFETY: fd 0 stays open for the life of the prog.
    let mut net = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    let mut req = vec![0u8; 1024];
    let n = net.read(&mut req)?;
    req.truncate(n);
    let r = serve(conf, &mut net, &req);
    let _ = std::fs::write(format!("{dir}/ctl"), "hangup");
    r
}

fn serve(conf: &Conf, net: &mut File, req: &[u8]) -> Result<(), Error> {
    let op = if req.len() >= 2 {
        u16::from_be_bytes([req[0], req[1]])
    } else {
        0
    };
    let mut fields = req.get(2..).unwrap_or(&[]).split(|&b| b == 0);
    let name = String::from_utf8_lossy(fields.next().unwrap_or(&[])).into_owned();
    let mode = String::from_utf8_lossy(fields.next().unwrap_or(&[])).to_ascii_lowercase();
    if (op != RRQ && op != WRQ) || name.is_empty() {
        return nak(net, EBADOP, "illegal TFTP operation");
    }
    if mode != "octet" && mode != "netascii" {
        return nak(net, EBADOP, "unknown transfer mode");
    }
    if conf.debug {
        let what = if op == RRQ { "read" } else { "write" };
        println!("tftpd: {what} {name} {mode}");
    }
    let Some(path) = resolve(conf, &name) else {
        return nak(net, EACCESS, "access violation");
    };
    if op == RRQ {
        send(conf, net, &path)
    } else {
        receive(conf, net, &path)
    }
}

/// The file `name` names, or `None` if it may not be used.
fn resolve(conf: &Conf, name: &str) -> Option<String> {
    if name.split('/').any(|e| e == "..") {
        return None;
    }
    let home = conf.home.trim_end_matches('/');
    Some(match name.strip_prefix('/') {
        Some(rest) if conf.restrict => format!("{home}/{rest}"),
        Some(_) => name.to_string(),
        None => format!("{home}/{name}"),
    })
}

fn send(conf: &Conf, net: &mut File, path: &str) -> Result<(), Error> {
    let Ok(d) = purgatorio::stat(path) else {
        return nak(net, ENOTFOUND, "file not found");
    };
    if d.mode & 0o004 == 0 || d.mode & purgatorio::DMDIR != 0 {
        return nak(net, EACCESS, "access violation");
    }
    let Ok(data) = std::fs::read(path) else {
        return nak(net, EACCESS, "access violation");
    };
    let mut block: u16 = 1;
    let mut chunks: Vec<&[u8]> = data.chunks(SEGSIZE).collect();
    // A file whose size is a multiple of the block ends with an empty one.
    if data.len().is_multiple_of(SEGSIZE) {
        chunks.push(&[]);
    }
    for chunk in chunks {
        let mut pkt = Vec::with_capacity(4 + chunk.len());
        pkt.extend_from_slice(&DATA.to_be_bytes());
        pkt.extend_from_slice(&block.to_be_bytes());
        pkt.extend_from_slice(chunk);
        if !exchange(net, &pkt, ACK, block)? {
            if conf.debug {
                println!("tftpd: {path}: no ack for block {block}");
            }
            return Ok(());
        }
        block = block.wrapping_add(1);
    }
    if conf.debug {
        println!("tftpd: sent {path}, {} bytes", data.len());
    }
    Ok(())
}

fn receive(conf: &Conf, net: &mut File, path: &str) -> Result<(), Error> {
    let writable = purgatorio::stat(path)
        .is_ok_and(|d| d.mode & 0o002 != 0 && d.mode & purgatorio::DMDIR == 0);
    if !writable {
        return nak(net, EACCESS, "access violation");
    }
    let mut out = Vec::new();
    let mut ack: u16 = 0;
    loop {
        let mut pkt = ACK.to_be_bytes().to_vec();
        pkt.extend_from_slice(&ack.to_be_bytes());
        let Some(data) = exchange_data(net, &pkt, ack.wrapping_add(1))? else {
            if conf.debug {
                println!("tftpd: {path}: no data after block {ack}");
            }
            return Ok(());
        };
        ack = ack.wrapping_add(1);
        out.extend_from_slice(&data);
        if data.len() < SEGSIZE {
            let mut last = ACK.to_be_bytes().to_vec();
            last.extend_from_slice(&ack.to_be_bytes());
            net.write_all(&last)?;
            break;
        }
    }
    if std::fs::write(path, &out).is_err() {
        return nak(net, EACCESS, "access violation");
    }
    if conf.debug {
        println!("tftpd: received {path}, {} bytes", out.len());
    }
    Ok(())
}

/// Sends `pkt` until the `want` packet for `block` comes back.
fn exchange(net: &mut File, pkt: &[u8], want: u16, block: u16) -> Result<bool, Error> {
    for _ in 0..TRIES {
        net.write_all(pkt)?;
        while let Some(r) = dial::read_timeout(net.as_raw_fd(), 1024, WAIT_MS)? {
            match header(&r) {
                Some((op, b)) if op == want && b == block => return Ok(true),
                Some((ERROR, _)) => return Ok(false),
                // A duplicate of an earlier packet: keep waiting.
                _ => {}
            }
        }
    }
    Ok(false)
}

/// Sends the ack `pkt` until data block `block` arrives; returns its data.
fn exchange_data(net: &mut File, pkt: &[u8], block: u16) -> Result<Option<Vec<u8>>, Error> {
    for _ in 0..TRIES {
        net.write_all(pkt)?;
        while let Some(r) = dial::read_timeout(net.as_raw_fd(), 1024, WAIT_MS)? {
            match header(&r) {
                Some((DATA, b)) if b == block => return Ok(Some(r[4..].to_vec())),
                Some((ERROR, _)) => return Ok(None),
                _ => {}
            }
        }
    }
    Ok(None)
}

fn header(p: &[u8]) -> Option<(u16, u16)> {
    if p.len() < 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([p[0], p[1]]),
        u16::from_be_bytes([p[2], p[3]]),
    ))
}

/// Sends an error packet.
fn nak(net: &mut File, code: u16, msg: &str) -> Result<(), Error> {
    let mut pkt = ERROR.to_be_bytes().to_vec();
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    net.write_all(&pkt)?;
    Ok(())
}
