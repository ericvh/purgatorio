//! ip/dhcp [-bdnpr] [-g gateway] [-h hostname] [-x net] ifcdir [localip [localmask]]:
//! get an address for the interface `ifcdir` (`/net/ipifc/0`) from a DHCP
//! server (RFC 2131), or a BOOTP one with `-b`, and configure it, as
//! Inferno's dhcp(8).
//!
//! dhcp reads the interface's hardware address from `ifcdir/status`,
//! announces `udp!*!68` in `headers` mode and broadcasts a discover to
//! port 67. It takes the first offer, requests it, and on the server's
//! acknowledgement writes `add ip mask` to `ifcdir/ctl` and, if the
//! server named a router (or `-g` gave one), `add 0.0.0.0 0.0.0.0 gw` to
//! `net/iproute`. `localip` asks for that address again.
//!
//! `-p` prints the configuration as an ndb(6) entry; `-d` traces the
//! exchange; `-n` configures nothing; `-r` retries until a server
//! answers; `-h` sends a host name; `-x` names the network's mount
//! point (`/net`). Without a reply the status is `fail:no reply`.
//!
//! Not done: renewing the lease (dhcp exits once configured), `-m`, and
//! writing `net/ndb`, which `#I` does not have.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, dial};

const USAGE: &str =
    "ip/dhcp [-bdnpr] [-g gateway] [-h hostname] [-x net] ifcdir [localip [localmask]]";
/// The size of the address header a `headers` mode UDP conversation puts
/// before each datagram (ip(3)).
const UDP_HEADER: usize = 52;
const COOKIE: [u8; 4] = [99, 130, 83, 99];
const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
/// How long each reply is waited for, and how many times each message
/// is sent.
const WAIT_MS: u64 = 2000;
const TRIES: usize = 3;

struct Opts {
    bootp: bool,
    debug: bool,
    configure: bool,
    print: bool,
    retry: bool,
    gateway: Option<[u8; 4]>,
    hostname: Option<String>,
    net: String,
    ifcdir: String,
    want: Option<[u8; 4]>,
}

/// What a server's reply gives.
#[derive(Default)]
struct Lease {
    kind: Option<u8>,
    addr: [u8; 4],
    mask: Option<[u8; 4]>,
    router: Option<[u8; 4]>,
    dns: Vec<[u8; 4]>,
    domain: Option<String>,
    hostname: Option<String>,
    lease: Option<u32>,
    server: Option<[u8; 4]>,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let usage = || Error(format!("usage: {USAGE}"));
    let mut o = Opts {
        bootp: false,
        debug: false,
        configure: true,
        print: false,
        retry: false,
        gateway: None,
        hostname: None,
        net: String::from("/net"),
        ifcdir: String::new(),
        want: None,
    };
    while let Some(c) = arg.opt() {
        match c {
            'b' => o.bootp = true,
            'd' => o.debug = true,
            'n' => o.configure = false,
            'p' => o.print = true,
            'r' => o.retry = true,
            'g' => o.gateway = Some(arg.arg().as_deref().and_then(ipv4).ok_or_else(usage)?),
            'h' => o.hostname = Some(arg.earg()?),
            'x' => o.net = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    match rest.as_slice() {
        [ifc] => o.ifcdir = ifc.clone(),
        [ifc, ip] | [ifc, ip, _] => {
            o.ifcdir = ifc.clone();
            o.want = Some(ipv4(ip).ok_or_else(usage)?);
        }
        _ => return Err(usage()),
    }
    let mac = hardware_address(&o.ifcdir)?;
    let (conv, mut data) = announce(&o.net)?;
    let lease = loop {
        match exchange(&o, &mac, &mut data)? {
            Some(l) => break l,
            None if o.retry => std::thread::sleep(Duration::from_secs(5)),
            None => {
                eprintln!("ip/dhcp: no reply from a server");
                purgatorio::exits("fail:no reply");
            }
        }
    };
    drop(data);
    drop(conv);
    if o.configure {
        configure(&o, &lease)?;
    }
    if o.print {
        print!("{}", ndb_entry(&lease, o.gateway));
    }
    Ok(())
}

/// The interface's hardware address: the third field of the first line
/// of `ifcdir/status`, after the device and the MTU.
fn hardware_address(ifcdir: &str) -> Result<[u8; 6], Error> {
    let path = format!("{ifcdir}/status");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error(format!("can't read {path}: {}", Error::from(e).0)))?;
    let field = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(2))
        .unwrap_or("");
    let hex: String = field.chars().filter(|c| *c != ':').collect();
    if hex.len() != 12 {
        return Err(Error(format!("{path}: no hardware address")));
    }
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| Error(format!("{path}: bad hardware address")))?;
    }
    Ok(mac)
}

/// Announces `udp!*!68` in `headers` mode: the ctl file (held open) and
/// the data file.
fn announce(net: &str) -> Result<(File, File), Error> {
    let mut ctl = rw(&format!("{net}/udp/clone"))?;
    let mut buf = [0u8; 32];
    let n = ctl.read(&mut buf)?;
    let id = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    for cmd in ["announce *!68", "headers"] {
        ctl.write_all(cmd.as_bytes())
            .map_err(|e| Error(format!("can't {cmd} udp: {}", Error::from(e).0)))?;
    }
    let data = rw(&format!("{net}/udp/{id}/data"))?;
    Ok((ctl, data))
}

fn rw(path: &str) -> Result<File, Error> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error(format!("can't open {path}: {}", Error::from(e).0)))
}

/// One discover, offer, request and acknowledgement (or one BOOTP
/// request and reply). `None` if no server answered.
fn exchange(o: &Opts, mac: &[u8; 6], data: &mut File) -> Result<Option<Lease>, Error> {
    let xid = transaction_id();
    if o.bootp {
        let m = message(o, mac, xid, None, o.want, None);
        return ask(o, data, &m, xid, mac, &[]);
    }
    let discover = message(o, mac, xid, Some(DISCOVER), o.want, None);
    let Some(offer) = ask(o, data, &discover, xid, mac, &[OFFER])? else {
        return Ok(None);
    };
    if o.debug {
        println!(
            "dhcp: offer {} from {}",
            text(offer.addr),
            offer.server.map(text).unwrap_or_default()
        );
    }
    let request = message(o, mac, xid, Some(REQUEST), Some(offer.addr), offer.server);
    let Some(ack) = ask(o, data, &request, xid, mac, &[ACK, NAK])? else {
        return Ok(None);
    };
    if ack.kind == Some(NAK) {
        if o.debug {
            println!("dhcp: refused");
        }
        return Ok(None);
    }
    Ok(Some(ack))
}

/// Broadcasts `m` until a reply of one of `kinds` (any reply, for BOOTP)
/// to this transaction comes.
fn ask(
    o: &Opts,
    data: &mut File,
    m: &[u8],
    xid: u32,
    mac: &[u8; 6],
    kinds: &[u8],
) -> Result<Option<Lease>, Error> {
    let mut packet = header([255, 255, 255, 255], 67).to_vec();
    packet.extend_from_slice(m);
    for _ in 0..TRIES {
        if o.debug {
            println!("dhcp: send {}", kind_name(m));
        }
        data.write_all(&packet)?;
        let deadline = std::time::Instant::now() + Duration::from_millis(WAIT_MS);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let Some(r) = dial::read_timeout(data.as_raw_fd(), 65536, left.as_millis() as u64)?
            else {
                break;
            };
            let Some(lease) = r.get(UDP_HEADER..).and_then(|b| parse(b, xid, mac)) else {
                continue;
            };
            if kinds.is_empty() || lease.kind.is_some_and(|k| kinds.contains(&k)) {
                return Ok(Some(lease));
            }
        }
    }
    Ok(None)
}

/// A `headers` mode header for a datagram to `addr!port`.
fn header(addr: [u8; 4], port: u16) -> [u8; UDP_HEADER] {
    let mut h = [0u8; UDP_HEADER];
    h[10] = 0xff;
    h[11] = 0xff;
    h[12..16].copy_from_slice(&addr);
    h[48..50].copy_from_slice(&port.to_be_bytes());
    h[50..52].copy_from_slice(&68u16.to_be_bytes());
    h
}

fn transaction_id() -> u32 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (t.subsec_nanos() ^ (t.as_secs() as u32)).wrapping_mul(2_654_435_761) ^ purgatorio::pid()
}

/// A BOOTREQUEST: a DHCP message of type `kind`, or plain BOOTP.
fn message(
    o: &Opts,
    mac: &[u8; 6],
    xid: u32,
    kind: Option<u8>,
    requested: Option<[u8; 4]>,
    server: Option<[u8; 4]>,
) -> Vec<u8> {
    let mut m = vec![0u8; 236];
    m[0] = BOOTREQUEST;
    m[1] = 1; // Ethernet
    m[2] = 6;
    m[4..8].copy_from_slice(&xid.to_be_bytes());
    // Ask for broadcast replies: the interface may not have the address.
    m[10] = 0x80;
    m[28..34].copy_from_slice(mac);
    m.extend_from_slice(&COOKIE);
    let mut opt = |code: u8, value: &[u8]| {
        m.push(code);
        m.push(value.len() as u8);
        m.extend_from_slice(value);
    };
    if let Some(kind) = kind {
        opt(53, &[kind]);
        let mut id = vec![1u8];
        id.extend_from_slice(mac);
        opt(61, &id);
        if let Some(addr) = requested {
            opt(50, &addr);
        }
        if let Some(addr) = server {
            opt(54, &addr);
        }
        opt(55, &[1, 3, 6, 12, 15, 28, 51, 54]);
    }
    if let Some(name) = &o.hostname {
        opt(12, &name.as_bytes()[..name.len().min(255)]);
    }
    m.push(255);
    // BOOTP messages are at least 300 bytes.
    if m.len() < 300 {
        m.resize(300, 0);
    }
    m
}

/// A BOOTREPLY to transaction `xid` for `mac`.
fn parse(m: &[u8], xid: u32, mac: &[u8; 6]) -> Option<Lease> {
    if m.len() < 236 || m[0] != BOOTREPLY || m[4..8] != xid.to_be_bytes() || m[28..34] != *mac {
        return None;
    }
    let mut l = Lease {
        addr: m[16..20].try_into().ok()?,
        ..Lease::default()
    };
    if m.get(236..240) != Some(&COOKIE[..]) {
        return Some(l);
    }
    let addr = |v: &[u8]| -> Option<[u8; 4]> { v.get(..4)?.try_into().ok() };
    let mut i = 240;
    while i < m.len() {
        let code = m[i];
        if code == 0 {
            i += 1;
            continue;
        }
        if code == 255 || i + 1 >= m.len() {
            break;
        }
        let len = usize::from(m[i + 1]);
        let v = m.get(i + 2..i + 2 + len)?;
        match code {
            1 => l.mask = addr(v),
            3 => l.router = addr(v),
            6 => l.dns = v.as_chunks::<4>().0.to_vec(),
            12 => l.hostname = Some(String::from_utf8_lossy(v).into_owned()),
            15 => l.domain = Some(String::from_utf8_lossy(v).into_owned()),
            51 if len == 4 => l.lease = Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            53 if len == 1 => l.kind = Some(v[0]),
            54 => l.server = addr(v),
            _ => {}
        }
        i += 2 + len;
    }
    Some(l)
}

/// Sets the interface's address and the default route.
fn configure(o: &Opts, l: &Lease) -> Result<(), Error> {
    let mask = l.mask.unwrap_or_else(|| class_mask(l.addr));
    let ctl = format!("{}/ctl", o.ifcdir);
    let cmd = format!("add {} {}", text(l.addr), text(mask));
    write_file(&ctl, &cmd)?;
    if let Some(gw) = l.router.or(o.gateway) {
        let route = format!("{}/iproute", o.net);
        write_file(&route, &format!("add 0.0.0.0 0.0.0.0 {}", text(gw)))?;
    }
    Ok(())
}

fn write_file(path: &str, cmd: &str) -> Result<(), Error> {
    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| Error(format!("can't open {path}: {}", Error::from(e).0)))?;
    f.write_all(cmd.as_bytes())
        .map_err(|e| Error(format!("{path}: {cmd}: {}", Error::from(e).0)))
}

/// The configuration as an ndb(6) entry.
fn ndb_entry(l: &Lease, gateway: Option<[u8; 4]>) -> String {
    let mask = l.mask.unwrap_or_else(|| class_mask(l.addr));
    let mut s = format!("ip={} ipmask={}", text(l.addr), text(mask));
    if let Some(gw) = l.router.or(gateway) {
        s.push_str(&format!(" ipgw={}", text(gw)));
    }
    s.push('\n');
    if !l.dns.is_empty() {
        let dns: Vec<String> = l.dns.iter().map(|a| format!("dns={}", text(*a))).collect();
        s.push_str(&format!("\t{}\n", dns.join(" ")));
    }
    if let Some(d) = &l.domain {
        s.push_str(&format!("\tdnsdomain={d}\n"));
    }
    if let Some(h) = &l.hostname {
        s.push_str(&format!("\tsys={h}\n"));
    }
    let mut tail = Vec::new();
    if let Some(server) = l.server {
        tail.push(format!("dhcp={}", text(server)));
    }
    if let Some(t) = l.lease {
        tail.push(format!("lease={t}"));
    }
    if !tail.is_empty() {
        s.push_str(&format!("\t{}\n", tail.join(" ")));
    }
    s
}

fn kind_name(m: &[u8]) -> &'static str {
    let kind = m
        .get(240..)
        .and_then(|o| o.windows(3).find(|w| w[0] == 53 && w[1] == 1))
        .map(|w| w[2]);
    match kind {
        Some(DISCOVER) => "discover",
        Some(REQUEST) => "request",
        _ => "bootp request",
    }
}

/// The mask of the address's class, when the server gives none.
fn class_mask(a: [u8; 4]) -> [u8; 4] {
    match a[0] {
        0..=127 => [255, 0, 0, 0],
        128..=191 => [255, 255, 0, 0],
        _ => [255, 255, 255, 0],
    }
}

fn ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut out {
        *b = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn text(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Opts {
        Opts {
            bootp: false,
            debug: false,
            configure: false,
            print: true,
            retry: false,
            gateway: None,
            hostname: Some(String::from("kremvax")),
            net: String::from("/net"),
            ifcdir: String::from("/net/ipifc/0"),
            want: None,
        }
    }

    #[test]
    fn discover_and_reply() {
        let mac = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
        let m = message(&opts(), &mac, 7, Some(DISCOVER), None, None);
        assert_eq!(m.len(), 300);
        assert_eq!((m[0], m[1], m[2]), (1, 1, 6));
        assert_eq!(&m[236..243], &[99, 130, 83, 99, 53, 1, DISCOVER]);
        assert_eq!(kind_name(&m), "discover");
        // The server's offer: the same transaction, options after the cookie.
        let mut r = m.clone();
        r[0] = BOOTREPLY;
        r[16..20].copy_from_slice(&[10, 0, 2, 15]);
        r.truncate(240);
        for o in [
            &[53, 1, OFFER][..],
            &[1, 4, 255, 255, 255, 0],
            &[3, 4, 10, 0, 2, 2],
            &[6, 4, 10, 0, 2, 3],
            &[51, 4, 0, 1, 0x51, 0x80],
            &[54, 4, 10, 0, 2, 2],
            &[255],
        ] {
            r.extend_from_slice(o);
        }
        let l = parse(&r, 7, &mac).unwrap();
        assert_eq!(l.kind, Some(OFFER));
        assert_eq!(
            ndb_entry(&l, None),
            "ip=10.0.2.15 ipmask=255.255.255.0 ipgw=10.0.2.2\n\tdns=10.0.2.3\n\tdhcp=10.0.2.2 lease=86400\n"
        );
        assert!(parse(&r, 8, &mac).is_none());
    }
}
