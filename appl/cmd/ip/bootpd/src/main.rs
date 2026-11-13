//! ip/bootpd [-dqsv] [-f dbfile] [-x net] [-a addr]: answer BOOTP and
//! DHCP requests from the network database, as Inferno's bootpd(8).
//!
//! bootpd announces `udp!*!67` on `net` (default `/net`) in `headers`
//! mode and answers each request whose hardware address is the `ether`
//! of an entry in `dbfile` (default `/lib/ndb/local`), and that entry
//! has an `ip`. When the client already has an address (`ciaddr`), the
//! entry must have both. The reply carries the address, the server's own
//! address and name, and the boot file (`bootf`). The subnet mask
//! (`ipmask`), gateway (`ipgw`), file server (`fs` or `FILESERVER`),
//! authentication server (`auth` or `SIGNER`), `bootf`, `dns`,
//! `dnsdomain` and `lease` come from the entry, else from the `ipnet`
//! entries whose networks hold the address, the smallest network first.
//! A value may be a system name, looked up as `sys=name`. The database is
//! read again when it changes.
//!
//! A plain BOOTP request (no RFC 1048 cookie) gets Inferno's vendor area,
//! `p9  mask fs auth gateway`. One with the cookie gets RFC 2132 options
//! instead: mask, router, DNS servers, host name (`sys`) and domain
//! (`dnsdomain`). One that also has a DHCP message type (RFC 2131) is a
//! DHCP request: a discover gets an offer, a request an acknowledgement
//! of the database's address (or a refusal when the client asks for
//! another), an inform an acknowledgement without an address. The lease
//! is `lease` seconds, else a day. Leases are the database's static
//! addresses: nothing is allocated from a pool.
//!
//! Replies go to the relay (`giaddr`) when there is one, else to the
//! client's address if it has one, else are broadcast to port 68. Each
//! reply is noted on standard output (`bootpd: ether -> sys ip`) unless
//! `-q`; `-d` traces requests on standard error (twice, their contents);
//! `-s` only prints the requests it sees.
//!
//! `-a addr` (Purgatorio's addition, for tests on loopback) announces
//! `addr` instead; replies then go back to the sender's address and
//! port, as Inferno's test mode did, the server address is the
//! announced host, and with `-d` the announced address is printed on
//! standard output first. The ARP entry Inferno's bootpd adds for the
//! client is not added: `#I` has no `arp` file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

use ndb::Entry;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error, dial};

const USAGE: &str = "ip/bootpd [-dqsv] [-f dbfile] [-x net] [-a addr]";
/// The address header of a `headers` mode UDP conversation (ip(3)).
const UDP_HEADER: usize = 52;
/// The fixed part of a BOOTP message, vendor area included.
const BOOTP_LEN: usize = 300;
const COOKIE: [u8; 4] = [99, 130, 83, 99];
const P9_COOKIE: [u8; 4] = *b"p9  ";
const DAY: u32 = 86400;

// DHCP message types (RFC 2132 option 53).
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const DECLINE: u8 = 4;
const ACK: u8 = 5;
const NAK: u8 = 6;
const RELEASE: u8 = 7;
const INFORM: u8 = 8;

struct Opts {
    debug: u32,
    sniff: bool,
    verbose: bool,
    file: String,
    net: String,
    addr: Option<String>,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut o = Opts {
        debug: 0,
        sniff: false,
        verbose: true,
        file: String::from("/lib/ndb/local"),
        net: String::from("/net"),
        addr: None,
    };
    while let Some(c) = arg.opt() {
        match c {
            'd' => o.debug += 1,
            's' => {
                o.sniff = true;
                o.debug = 255;
            }
            'q' => o.verbose = false,
            'v' => o.verbose = true,
            'f' => o.file = arg.earg()?,
            'x' => o.net = arg.earg()?,
            'a' => o.addr = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let mut db = Db::new(&o.file);
    if !o.sniff
        && let Err(e) = db.read()
    {
        fatal(&e);
    }
    let myname = sysname();
    if myname.is_empty() {
        fatal("system name not set");
    }
    let addr = o.addr.clone().unwrap_or_else(|| String::from("udp!*!67"));
    let (_ctl, mut data, local) = match announce(&o.net, &addr) {
        Ok(a) => a,
        Err(e) => fatal(&format!("can't announce {addr}: {e}")),
    };
    let testing = o.addr.is_some();
    let siaddr = match server_addr(&local, testing, &myname) {
        Ok(a) => a,
        Err(e) => fatal(&format!("can't find IP address for {myname}: {e}")),
    };
    if o.debug > 0 {
        eprintln!("bootpd: local IP address is {}", text(siaddr));
    }
    if testing && o.debug > 0 {
        println!("bootpd: announced {local}");
    }
    let mut buf = vec![0u8; 2048];
    let mut badread = 0;
    loop {
        let n = match data.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => {
                let e = Error::from(e);
                if e.0 == purgatorio::abi::error::EINTR {
                    return Ok(());
                }
                badread += 1;
                if badread > 10 {
                    eprintln!("bootpd: {badread} read errors: {e}");
                    return Ok(());
                }
                continue;
            }
        };
        badread = 0;
        if n < UDP_HEADER {
            if o.debug > 0 {
                eprintln!("bootpd: short Udphdr: {n} bytes");
            }
            continue;
        }
        let (hdr, body) = buf[..n].split_at(UDP_HEADER);
        let (raddr, rport) = header_remote(hdr);
        if o.debug > 0 {
            eprintln!("bootpd: received request from udp!{}!{rport}", text(raddr));
        }
        if body.len() < BOOTP_LEN {
            if o.debug > 0 {
                eprintln!("bootpd: short request of {} bytes", body.len());
            }
            continue;
        }
        let req = match Bootp::unpack(body) {
            Ok(b) => b,
            Err(e) => {
                if o.debug > 0 {
                    eprintln!("bootpd: can't unpack packet: {e}");
                }
                continue;
            }
        };
        if o.debug >= 2 {
            eprintln!("bootpd: recvd {{{}}}", req.text());
        }
        if o.sniff || req.op != 1 {
            continue;
        }
        if req.htype != 1 || req.hlen != 6 {
            if o.debug > 0 {
                eprintln!("bootpd: hardware type not ether; ignoring.");
            }
            continue;
        }
        if let Err(e) = db.read() {
            eprintln!("bootpd: getreply: dbread failed: {e}");
            continue;
        }
        let Some(rec) = db.lookup(&req) else {
            if o.debug > 0 {
                eprintln!("bootpd: cannot answer request.");
            }
            continue;
        };
        if o.debug > 0 {
            eprintln!("bootpd: found a matching entry: {{{}}}", rec.text());
        }
        let Some(reply) = mkreply(&req, &rec, siaddr, &myname) else {
            continue;
        };
        if o.verbose {
            let what = match reply.dhcp {
                Some(t) => format!(" ({})", dhcp_name(t)),
                None => String::new(),
            };
            println!(
                "bootpd: {} -> {} {}{what}",
                ether_text(&rec.ha),
                rec.hostname,
                text(rec.ip)
            );
        }
        if o.debug > 0 {
            eprintln!("bootpd: reply {{{}}}", reply.bootp.text());
        }
        let (to, port) = if testing {
            (raddr, rport)
        } else if req.giaddr != [0; 4] {
            (req.giaddr, 67)
        } else if ipok(req.ciaddr) {
            (req.ciaddr, 68)
        } else {
            ([255; 4], 68)
        };
        let mut out = hdr.to_vec();
        out[..10].fill(0);
        out[10] = 0xff;
        out[11] = 0xff;
        out[12..16].copy_from_slice(&to);
        out[48..50].copy_from_slice(&port.to_be_bytes());
        out.extend_from_slice(&reply.bootp.pack());
        if let Err(e) = data.write_all(&out) {
            eprintln!("bootpd: udp write error: {}", Error::from(e));
        }
    }
}

fn fatal(msg: &str) -> ! {
    eprintln!("bootpd: {msg}");
    purgatorio::exits("fail:error");
}

/// The system's name, from `/dev/sysname`.
fn sysname() -> String {
    std::fs::read_to_string("/dev/sysname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The address the replies give as the server's: the announced host in
/// test mode, else the system name as cs translates it.
fn server_addr(local: &str, testing: bool, name: &str) -> Result<[u8; 4], Error> {
    if testing
        && let Some(a) = local.rsplit_once('!').and_then(|(h, _)| parse_ip(h))
        && a != [0; 4]
    {
        return Ok(a);
    }
    let t = dial::translate(&format!("net!{name}!0"))?;
    let host = t
        .first()
        .and_then(|a| a.split('!').nth(1))
        .ok_or_else(|| Error::from("bad cs reply format"))?;
    parse_ip(host).ok_or_else(|| Error(format!("can't parse address: {host}")))
}

/// Announces `addr` on `net` in `headers` mode: the ctl file (held open),
/// the data file and the announced address.
fn announce(net: &str, addr: &str) -> Result<(File, File, String), Error> {
    let addr = dial::netmkaddr(addr);
    let parts: Vec<&str> = addr.splitn(3, '!').collect();
    let [proto, host, port] = parts.as_slice() else {
        return Err(Error(format!("bad address {addr}")));
    };
    let port = match port.parse::<u16>() {
        Ok(p) => p.to_string(),
        Err(_) => port_number(port)?,
    };
    let mut ctl = rw(&format!("{net}/{proto}/clone"))?;
    let mut idbuf = [0u8; 32];
    let k = ctl.read(&mut idbuf)?;
    let id = String::from_utf8_lossy(&idbuf[..k]).trim().to_string();
    ctl.write_all(format!("announce {host}!{port}").as_bytes())
        .map_err(Error::from)?;
    ctl.write_all(b"headers")
        .map_err(|e| Error(format!("can't set headers mode: {}", Error::from(e).0)))?;
    let dir = format!("{net}/{proto}/{id}");
    let data = rw(&format!("{dir}/data"))?;
    let local = std::fs::read_to_string(format!("{dir}/local"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
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

/// The remote address and port of a `headers` mode header.
fn header_remote(h: &[u8]) -> ([u8; 4], u16) {
    (
        [h[12], h[13], h[14], h[15]],
        u16::from_be_bytes([h[48], h[49]]),
    )
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut a = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut a {
        *b = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(a)
}

fn text(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn ipok(a: [u8; 4]) -> bool {
    a != [0; 4] && a != [255; 4] && !(224..240).contains(&a[0])
}

fn ether_text(ha: &[u8]) -> String {
    ha.iter().map(|b| format!("{b:02x}")).collect()
}

/// An `ether=` value in the form ether_text gives: colons and dashes
/// dropped, lower case.
fn ether_norm(s: &str) -> String {
    s.chars()
        .filter(|c| *c != ':' && *c != '-' && *c != '.')
        .flat_map(char::to_lowercase)
        .collect()
}

fn ctostr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// A BOOTP message (RFC 951), with any options after the RFC 1048
/// cookie.
#[derive(Clone)]
struct Bootp {
    op: u8,
    htype: u8,
    hlen: u8,
    hops: u8,
    xid: u32,
    secs: u16,
    flags: u16,
    ciaddr: [u8; 4],
    yiaddr: [u8; 4],
    siaddr: [u8; 4],
    giaddr: [u8; 4],
    chaddr: [u8; 16],
    sname: String,
    file: String,
    /// The vendor area: 64 bytes for plain BOOTP, else the cookie and
    /// the options.
    vend: Vec<u8>,
}

impl Bootp {
    fn unpack(d: &[u8]) -> Result<Bootp, &'static str> {
        if d.len() < BOOTP_LEN {
            return Err("too short");
        }
        if d[2] > 16 {
            return Err("length error");
        }
        let ip = |o: usize| [d[o], d[o + 1], d[o + 2], d[o + 3]];
        let mut chaddr = [0u8; 16];
        chaddr.copy_from_slice(&d[28..44]);
        Ok(Bootp {
            op: d[0],
            htype: d[1],
            hlen: d[2],
            hops: d[3],
            xid: u32::from_be_bytes(ip(4)),
            secs: u16::from_be_bytes([d[8], d[9]]),
            flags: u16::from_be_bytes([d[10], d[11]]),
            ciaddr: ip(12),
            yiaddr: ip(16),
            siaddr: ip(20),
            giaddr: ip(24),
            chaddr,
            sname: ctostr(&d[44..108]),
            file: ctostr(&d[108..236]),
            vend: d[236..].to_vec(),
        })
    }

    fn pack(&self) -> Vec<u8> {
        let mut d = vec![0u8; 236];
        d[0] = self.op;
        d[1] = self.htype;
        d[2] = self.hlen;
        d[3] = self.hops;
        d[4..8].copy_from_slice(&self.xid.to_be_bytes());
        d[8..10].copy_from_slice(&self.secs.to_be_bytes());
        d[10..12].copy_from_slice(&self.flags.to_be_bytes());
        d[12..16].copy_from_slice(&self.ciaddr);
        d[16..20].copy_from_slice(&self.yiaddr);
        d[20..24].copy_from_slice(&self.siaddr);
        d[24..28].copy_from_slice(&self.giaddr);
        d[28..44].copy_from_slice(&self.chaddr);
        let sname = self.sname.as_bytes();
        let n = sname.len().min(63);
        d[44..44 + n].copy_from_slice(&sname[..n]);
        let file = self.file.as_bytes();
        let n = file.len().min(127);
        d[108..108 + n].copy_from_slice(&file[..n]);
        d.extend_from_slice(&self.vend);
        if d.len() < BOOTP_LEN {
            d.resize(BOOTP_LEN, 0);
        }
        d
    }

    fn ha(&self) -> &[u8] {
        &self.chaddr[..usize::from(self.hlen)]
    }

    /// The options after the RFC 1048 cookie, as (code, value).
    fn options(&self) -> Option<Vec<(u8, Vec<u8>)>> {
        if self.vend.get(..4)? != COOKIE {
            return None;
        }
        let mut out = Vec::new();
        let mut i = 4;
        while i < self.vend.len() {
            let code = self.vend[i];
            if code == 0 {
                i += 1;
                continue;
            }
            if code == 255 || i + 1 >= self.vend.len() {
                break;
            }
            let n = usize::from(self.vend[i + 1]);
            let Some(v) = self.vend.get(i + 2..i + 2 + n) else {
                break;
            };
            out.push((code, v.to_vec()));
            i += 2 + n;
        }
        Some(out)
    }

    fn option(&self, code: u8) -> Option<Vec<u8>> {
        self.options()?
            .into_iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| v)
    }

    fn magic(&self) -> String {
        let c = self.vend.get(..4).unwrap_or(&[]);
        if c == P9_COOKIE {
            String::from("plan9")
        } else if c == COOKIE {
            String::from("rfc1048")
        } else if c == b"CMU\0" {
            String::from("cmu")
        } else {
            let v: Vec<String> = c.iter().map(u8::to_string).collect();
            v.join(".")
        }
    }

    fn text(&self) -> String {
        let mut s = format!(
            "op={} htype={} hlen={} hops={} xid={} secs={} ciaddr={} yiaddr={}",
            self.op,
            self.htype,
            self.hlen,
            self.hops,
            self.xid,
            self.secs,
            text(self.ciaddr),
            text(self.yiaddr)
        );
        s.push_str(&format!(
            " server={} gateway={} hwaddr={} host={} file={} magic={}",
            text(self.siaddr),
            text(self.giaddr),
            ether_text(self.ha()),
            ndb::quote(&self.sname),
            ndb::quote(&self.file),
            self.magic()
        ));
        if self.magic() == "plan9" {
            s.push_str(&format!("({})", ctostr(&self.vend)));
        }
        s
    }
}

/// What the database says of a client.
#[derive(Default)]
struct Hostinfo {
    hostname: String,
    ha: Vec<u8>,
    ip: [u8; 4],
    bootf: Option<String>,
    netmask: Option<[u8; 4]>,
    ipgw: Option<[u8; 4]>,
    fs: Option<[u8; 4]>,
    auth: Option<[u8; 4]>,
    dns: Vec<[u8; 4]>,
    dom: Option<String>,
    lease: Option<u32>,
}

impl Hostinfo {
    fn text(&self) -> String {
        let o = |a: Option<[u8; 4]>| a.map_or_else(|| String::from("0.0.0.0"), text);
        format!(
            "ha={} ip={} bf={} sm={} gw={} fs={} au={}",
            ether_text(&self.ha),
            text(self.ip),
            self.bootf.as_deref().unwrap_or(""),
            o(self.netmask),
            o(self.ipgw),
            o(self.fs),
            o(self.auth)
        )
    }
}

/// The network database, read again when its file changes.
struct Db {
    file: String,
    entries: Option<Vec<Entry>>,
    mtime: u32,
}

impl Db {
    fn new(file: &str) -> Db {
        Db {
            file: String::from(file),
            entries: None,
            mtime: 0,
        }
    }

    fn read(&mut self) -> Result<(), String> {
        let mtime = purgatorio::stat(&self.file).map_or(0, |d| d.mtime);
        if self.entries.is_some() && mtime == self.mtime {
            return Ok(());
        }
        let read = |f: &str| {
            std::fs::read_to_string(f)
                .map_err(|_| format!("cannot open {f}: {}", purgatorio::errstr()))
        };
        self.entries = Some(ndb::open(&self.file, &read)?);
        self.mtime = mtime;
        Ok(())
    }

    fn entries(&self) -> &[Entry] {
        self.entries.as_deref().unwrap_or(&[])
    }

    /// The entry for a request: by its client address and hardware
    /// address together if it has an address, else by the hardware
    /// address alone.
    fn lookup(&self, req: &Bootp) -> Option<Hostinfo> {
        let ha = ether_text(req.ha());
        let has_ether = |e: &Entry| e.all("ether").iter().any(|v| ether_norm(v) == ha);
        if ipok(req.ciaddr) {
            let ip = text(req.ciaddr);
            if let Some(e) = self
                .entries()
                .iter()
                .find(|e| has_ether(e) && e.has("ip", Some(&ip)))
                && let Some(inf) = self.fill(e, Some(&ip))
            {
                return Some(inf);
            }
        }
        self.entries()
            .iter()
            .filter(|e| has_ether(e) && e.has("ip", None))
            .find_map(|e| self.fill(e, None))
    }

    fn fill(&self, e: &Entry, ip: Option<&str>) -> Option<Hostinfo> {
        let ipv = match ip {
            Some(ip) => ip.to_string(),
            None => e.get("ip")?.to_string(),
        };
        let addr = parse_ip(&ipv)?;
        let mut inf = Hostinfo {
            hostname: e.get("sys").unwrap_or("").to_string(),
            ha: e
                .get("ether")
                .map(ether_norm)
                .and_then(|s| parse_hex(&s))
                .unwrap_or_default(),
            ip: addr,
            ..Hostinfo::default()
        };
        // The host's own entry, then its networks, smallest first.
        let mut sources: Vec<&Entry> = vec![e];
        sources.extend(self.nets(addr));
        for s in sources {
            if inf.netmask.is_none() {
                inf.netmask = s.get("ipmask").and_then(parse_ip);
            }
            if inf.ipgw.is_none() {
                inf.ipgw = s.get("ipgw").and_then(|v| self.resolve(v));
            }
            if inf.fs.is_none() {
                inf.fs = s
                    .get("fs")
                    .or_else(|| s.get("FILESERVER"))
                    .and_then(|v| self.resolve(v));
            }
            if inf.auth.is_none() {
                inf.auth = s
                    .get("auth")
                    .or_else(|| s.get("SIGNER"))
                    .and_then(|v| self.resolve(v));
            }
            if inf.bootf.is_none() {
                inf.bootf = s.get("bootf").map(String::from);
            }
            if inf.dns.is_empty() {
                inf.dns = s
                    .all("dns")
                    .iter()
                    .filter_map(|v| self.resolve(v))
                    .collect();
            }
            if inf.dom.is_none() {
                inf.dom = s.get("dnsdomain").map(String::from);
            }
            if inf.lease.is_none() {
                inf.lease = s.get("lease").and_then(|v| v.parse().ok());
            }
        }
        Some(inf)
    }

    /// The network entries (those with `ipnet`, or an `ipmask`) whose
    /// network holds `addr`, the smallest network first.
    fn nets(&self, addr: [u8; 4]) -> Vec<&Entry> {
        let a = u32::from_be_bytes(addr);
        let mut nets: Vec<(u32, &Entry)> = self
            .entries()
            .iter()
            .filter(|e| e.has("ipnet", None) || e.has("ipmask", None))
            .filter_map(|e| {
                let net = u32::from_be_bytes(e.get("ip").and_then(parse_ip)?);
                let mask = match e.get("ipmask").and_then(parse_ip) {
                    Some(m) => u32::from_be_bytes(m),
                    None => class_mask(net),
                };
                (a & mask == net & mask).then_some((mask, e))
            })
            .collect();
        nets.sort_by_key(|x| std::cmp::Reverse(x.0));
        nets.into_iter().map(|(_, e)| e).collect()
    }

    /// An address, or a system name's address.
    fn resolve(&self, v: &str) -> Option<[u8; 4]> {
        parse_ip(v).or_else(|| {
            self.entries()
                .iter()
                .find(|e| e.has("sys", Some(v)))
                .and_then(|e| e.get("ip"))
                .and_then(parse_ip)
        })
    }
}

fn class_mask(a: u32) -> u32 {
    match a >> 24 {
        0..128 => 0xff00_0000,
        128..192 => 0xffff_0000,
        _ => 0xffff_ff00,
    }
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

struct Reply {
    bootp: Bootp,
    /// The DHCP message type, for a DHCP reply.
    dhcp: Option<u8>,
}

fn dhcp_name(t: u8) -> &'static str {
    match t {
        OFFER => "offer",
        ACK => "ack",
        NAK => "nak",
        _ => "?",
    }
}

/// The reply to `req` for the client `rec`, or `None` when a DHCP
/// message needs none.
fn mkreply(req: &Bootp, rec: &Hostinfo, siaddr: [u8; 4], myname: &str) -> Option<Reply> {
    let mut b = req.clone();
    b.op = 2;
    b.hops = 0;
    b.secs = 0;
    b.yiaddr = rec.ip;
    b.siaddr = siaddr;
    b.sname = String::from(myname);
    b.file = rec.bootf.clone().unwrap_or_default();
    let zero = |a: Option<[u8; 4]>| a.unwrap_or([0; 4]);
    let Some(opts) = req.options() else {
        // Plain BOOTP: Inferno's own vendor area.
        let v = format!(
            "p9  {} {} {} {}",
            text(zero(rec.netmask)),
            text(zero(rec.fs)),
            text(zero(rec.auth)),
            text(zero(rec.ipgw))
        );
        let mut vend = v.into_bytes();
        vend.resize(vend.len().max(64), 0);
        b.vend = vend;
        return Some(Reply {
            bootp: b,
            dhcp: None,
        });
    };
    let kind = opts
        .iter()
        .find(|(c, _)| *c == 53)
        .and_then(|(_, v)| v.first().copied());
    let mut o = Options::new();
    let dhcp = match kind {
        None => None,
        Some(DISCOVER) => Some(OFFER),
        Some(REQUEST) => {
            let asked = req
                .option(50)
                .and_then(|v| v.get(..4).map(|a| [a[0], a[1], a[2], a[3]]))
                .or_else(|| ipok(req.ciaddr).then_some(req.ciaddr));
            match asked {
                Some(a) if a != rec.ip => Some(NAK),
                _ => Some(ACK),
            }
        }
        Some(INFORM) => {
            b.yiaddr = [0; 4];
            Some(ACK)
        }
        Some(DECLINE | RELEASE) | Some(_) => return None,
    };
    if let Some(t) = dhcp {
        o.put(53, &[t]);
        o.put(54, &siaddr);
        if t == NAK {
            b.yiaddr = [0; 4];
            b.siaddr = [0; 4];
            b.file = String::new();
            b.sname = String::new();
            o.end();
            b.vend = o.0;
            return Some(Reply {
                bootp: b,
                dhcp: Some(t),
            });
        }
        if kind != Some(INFORM) {
            o.put(51, &rec.lease.unwrap_or(DAY).to_be_bytes());
        }
    }
    if let Some(m) = rec.netmask {
        o.put(1, &m);
    }
    if let Some(g) = rec.ipgw {
        o.put(3, &g);
    }
    if !rec.dns.is_empty() {
        let v: Vec<u8> = rec.dns.iter().flatten().copied().collect();
        o.put(6, &v);
    }
    if !rec.hostname.is_empty() {
        o.put(12, rec.hostname.as_bytes());
    }
    if let Some(d) = &rec.dom {
        o.put(15, d.as_bytes());
    }
    o.end();
    b.vend = o.0;
    Some(Reply { bootp: b, dhcp })
}

/// RFC 2132 options after the cookie.
struct Options(Vec<u8>);

impl Options {
    fn new() -> Options {
        Options(COOKIE.to_vec())
    }

    fn put(&mut self, code: u8, v: &[u8]) {
        let n = v.len().min(255);
        self.0.push(code);
        self.0.push(n as u8);
        self.0.extend_from_slice(&v[..n]);
    }

    fn end(&mut self) {
        self.0.push(255);
        if self.0.len() < 64 {
            self.0.resize(64, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(ether: [u8; 6], opts: &[(u8, &[u8])]) -> Bootp {
        let mut d = vec![0u8; BOOTP_LEN];
        d[0] = 1;
        d[1] = 1;
        d[2] = 6;
        d[4..8].copy_from_slice(&7u32.to_be_bytes());
        d[28..34].copy_from_slice(&ether);
        if !opts.is_empty() {
            let mut o = Options::new();
            for (c, v) in opts {
                o.put(*c, v);
            }
            o.end();
            d.truncate(236);
            d.extend_from_slice(&o.0);
            d.resize(BOOTP_LEN, 0);
        }
        Bootp::unpack(&d).unwrap()
    }

    fn db() -> Db {
        let text = "sys=fs ip=10.0.0.2 ether=00:a0:c9:e1:b2:c3 bootf=/kern\n\
                    sys=gate ip=10.0.0.1\n\
                    ipnet=lan ip=10.0.0.0 ipmask=255.255.255.0\n\
                    \tipgw=gate fs=fs dns=10.0.0.1\n\
                    ipnet=big ip=10.0.0.0 ipmask=255.0.0.0 auth=10.9.9.9 ipgw=10.0.0.254\n";
        Db {
            file: String::new(),
            entries: Some(ndb::parse(text, "t").unwrap()),
            mtime: 0,
        }
    }

    #[test]
    fn lookup_and_nets() {
        let db = db();
        let req = request([0, 0xa0, 0xc9, 0xe1, 0xb2, 0xc3], &[]);
        let inf = db.lookup(&req).unwrap();
        assert_eq!(
            inf.text(),
            "ha=00a0c9e1b2c3 ip=10.0.0.2 bf=/kern sm=255.255.255.0 gw=10.0.0.1 fs=10.0.0.2 au=10.9.9.9"
        );
        assert!(db.lookup(&request([1, 2, 3, 4, 5, 6], &[])).is_none());
    }

    #[test]
    fn replies() {
        let db = db();
        let req = request([0, 0xa0, 0xc9, 0xe1, 0xb2, 0xc3], &[]);
        let inf = db.lookup(&req).unwrap();
        let r = mkreply(&req, &inf, [10, 0, 0, 1], "gate").unwrap();
        assert!(r.dhcp.is_none());
        assert_eq!(
            ctostr(&r.bootp.vend),
            "p9  255.255.255.0 10.0.0.2 10.9.9.9 10.0.0.1"
        );
        let p = r.bootp.pack();
        assert_eq!(p.len(), BOOTP_LEN);
        assert_eq!(&p[16..20], &[10, 0, 0, 2]);

        let req = request([0, 0xa0, 0xc9, 0xe1, 0xb2, 0xc3], &[(53, &[DISCOVER])]);
        let r = mkreply(&req, &inf, [10, 0, 0, 1], "gate").unwrap();
        assert_eq!(r.dhcp, Some(OFFER));
        assert_eq!(r.bootp.option(1), Some(vec![255, 255, 255, 0]));
        assert_eq!(r.bootp.option(51), Some(DAY.to_be_bytes().to_vec()));
        let req = request(
            [0, 0xa0, 0xc9, 0xe1, 0xb2, 0xc3],
            &[(53, &[REQUEST]), (50, &[10, 0, 0, 3])],
        );
        let r = mkreply(&req, &inf, [10, 0, 0, 1], "gate").unwrap();
        assert_eq!(r.dhcp, Some(NAK));
        assert_eq!(r.bootp.yiaddr, [0; 4]);
    }
}
