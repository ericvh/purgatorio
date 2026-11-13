//! ip/rip [-2bdn] [-x mntpt] [-a addr] [net ...]: the routing information
//! protocol (RFC 1058, RFC 2453), as Inferno's rip(8).
//!
//! rip learns the interfaces and their networks from `mntpt/ipifc`
//! (default `/net`), notes the routes already in `mntpt/iproute`, and
//! announces `udp!*!rip` (port 520) in `headers` mode. It reads the
//! responses gateways send, version 1 or 2, and keeps a route to each
//! destination they offer through the gateway with the smallest metric:
//! a better route replaces a worse one, one whose metric reaches 16
//! (unreachable) from its own gateway is withdrawn, and routes to its own
//! networks are never touched. Each change is written to `iproute` as
//! `add dest mask gateway` or `remove dest mask`; `-n` changes nothing
//! and `-d` says on standard output what it changes (`-dd` also lists
//! every route in each packet, on standard error). The default route is
//! never changed.
//!
//! With `-b`, or with `net` addresses (which limit it to the interfaces
//! on those networks), rip asks the gateways for their tables at the
//! start and broadcasts its own changed routes every 60 seconds, as
//! version 1 or with `-2` version 2.
//!
//! `#I` keeps only the interfaces' networks and the default route, so on
//! Purgatorio an `add` for any other route is refused (`-d` says so) and
//! rip offers the route again at its next update. `-a addr` (Purgatorio's
//! addition, for tests on loopback) announces `addr` instead of port 520;
//! then responses from any port are taken, only those from the announced
//! address itself are ignored as its own, and with `-d` the announced
//! address is printed first.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, Ready, dial};

const USAGE: &str = "ip/rip [-2bdn] [-x mntpt] [-a addr] [net ...]";
/// The address header of a `headers` mode UDP conversation (ip(3)).
const UDP_HEADER: usize = 52;
const RIP_HEADER: usize = 4;
const ENTRY: usize = 20;
const MAX_RIP: usize = 512;
const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
/// The metric the protocol calls infinity.
const HOP_LIMIT: u32 = 16;
const RIP_PORT: u16 = 520;
const EXPIRED: u64 = 180;
const DISCARD: u64 = 240;
/// Seconds between broadcasts of the table.
const OUTPUT_RATE: u64 = 60;
const NETWORK_COST: u32 = 1;
const AF_INET: u16 = 2;

/// An interface address: the address, its mask and its network.
#[derive(Clone, Copy)]
struct Ifc {
    ip: u32,
    mask: u32,
    net: u32,
}

#[derive(Clone, Copy, Default)]
struct Gateway {
    dest: u32,
    mask: u32,
    gateway: u32,
    metric: u32,
    valid: bool,
    changed: bool,
    local: bool,
    time: u64,
}

struct Rip {
    debug: u32,
    nochange: bool,
    version: u8,
    nets: Vec<Ifc>,
    only: Vec<u32>,
    routes: Vec<Gateway>,
    defroute: Option<usize>,
    route: Option<File>,
    now: u64,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut quiet = true;
    let mut netdir = String::from("/net");
    let mut addr: Option<String> = None;
    let mut r = Rip {
        debug: 0,
        nochange: false,
        version: 1,
        nets: Vec::new(),
        only: Vec::new(),
        routes: Vec::new(),
        defroute: None,
        route: None,
        now: now(),
    };
    while let Some(c) = arg.opt() {
        match c {
            'd' => r.debug += 1,
            'b' => quiet = false,
            '2' => r.version = 2,
            'n' => r.nochange = true,
            'x' => netdir = arg.earg()?,
            'a' => addr = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    for a in arg.argv() {
        quiet = false;
        match parse_ip(&a) {
            Some(ip) => r.only.push(ip),
            None => fatal(&format!("invalid address: {a}")),
        }
    }
    r.nets = interfaces(&netdir);
    r.addlocal();
    let path = format!("{netdir}/iproute");
    match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => r.route = Some(f),
        Err(e) => fatal(&format!("can't open {path}: {}", Error::from(e))),
    }
    r.readroutes(&std::fs::read_to_string(&path).unwrap_or_default());
    syslog("started");
    let testing = addr.is_some();
    let want = addr.unwrap_or_else(|| format!("{netdir}/udp!*!rip"));
    let (_ctl, mut data, local) = match announce(&netdir, &want) {
        Ok(a) => a,
        Err(e) => fatal(&format!("can't announce {want}: {e}")),
    };
    if testing && r.debug > 0 {
        syslog(&format!("announced {local}"));
    }
    let me = local
        .rsplit_once('!')
        .and_then(|(h, p)| Some((parse_ip(h)?, p.parse::<u16>().ok()?)));
    let mut next_send = None;
    if !quiet {
        r.sendall(&mut data, OP_REQUEST, false);
        next_send = Some(now() + OUTPUT_RATE);
    }
    let mut buf = vec![0u8; 8192];
    loop {
        if let Some(t) = next_send {
            let wait = t.saturating_sub(now()) * 1000;
            let fds = [Ready::Read(data.as_raw_fd())];
            let ready = purgatorio::ready(&fds, i32::try_from(wait).unwrap_or(i32::MAX))?;
            if ready.is_none() {
                r.sendall(&mut data, OP_REPLY, true);
                next_send = Some(now() + OUTPUT_RATE);
                continue;
            }
        }
        let n = match data.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(_) => return Ok(()),
        };
        if n < UDP_HEADER + RIP_HEADER {
            continue;
        }
        let h = &buf[..UDP_HEADER];
        let raddr = u32::from_be_bytes([h[12], h[13], h[14], h[15]]);
        let rport = u16::from_be_bytes([h[48], h[49]]);
        let hdr = &buf[UDP_HEADER..n];
        let version = hdr[1];
        if version < 1 {
            continue;
        }
        match hdr[0] {
            OP_REQUEST => {}
            OP_REPLY => {
                if testing {
                    if me == Some((raddr, rport)) {
                        continue;
                    }
                } else if rport != RIP_PORT || r.ismyaddr(raddr) {
                    continue;
                }
                r.now = now();
                if r.debug > 1 {
                    eprintln!("from {}:", text(raddr));
                }
                for e in hdr[RIP_HEADER..].as_chunks::<ENTRY>().0 {
                    r.unpackroute(e, version, raddr);
                }
            }
            op => {
                if r.debug > 0 {
                    syslog(&format!("unexpected op: {op}"));
                }
            }
        }
    }
}

fn syslog(s: &str) {
    println!("rip: {s}");
}

fn fatal(s: &str) -> ! {
    syslog(s);
    purgatorio::exits("fail:error");
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn parse_ip(s: &str) -> Option<u32> {
    let mut a = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut a {
        *b = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(u32::from_be_bytes(a))
}

fn text(a: u32) -> String {
    let b = a.to_be_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

fn class_mask(a: u32) -> u32 {
    match a >> 24 {
        0..128 => 0xff00_0000,
        128..192 => 0xffff_0000,
        _ => 0xffff_ff00,
    }
}

/// The IPv4 interface addresses, from each `ipifc/n/status`: its second
/// line is `addr mask net ...`.
fn interfaces(netdir: &str) -> Vec<Ifc> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(format!("{netdir}/ipifc")) else {
        return out;
    };
    let mut names: Vec<String> = dirs
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.parse::<u32>().is_ok())
        .collect();
    names.sort_by_key(|n| n.parse::<u32>().unwrap_or(0));
    for n in names {
        let Ok(s) = std::fs::read_to_string(format!("{netdir}/ipifc/{n}/status")) else {
            continue;
        };
        for line in s.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let [ip, mask, net, ..] = f.as_slice()
                && let (Some(ip), Some(mask), Some(net)) =
                    (parse_ip(ip), parse_ip(mask), parse_ip(net))
                && ip != 0
            {
                out.push(Ifc { ip, mask, net });
            }
        }
    }
    out
}

/// Announces `addr` in `headers` mode: the ctl file (held open), the
/// data file and the announced address.
fn announce(netdir: &str, addr: &str) -> Result<(File, File, String), Error> {
    let addr = dial::netmkaddr(addr);
    let parts: Vec<&str> = addr.splitn(3, '!').collect();
    let [proto, host, port] = parts.as_slice() else {
        return Err(Error(format!("bad address {addr}")));
    };
    let proto = proto.rsplit('/').next().unwrap_or(proto);
    let port = match port.parse::<u16>() {
        Ok(p) => p.to_string(),
        Err(_) => {
            let t = dial::translate(&format!("udp!*!{port}"))?;
            t.first()
                .and_then(|a| a.rsplit('!').next())
                .filter(|p| p.parse::<u16>().is_ok())
                .map(String::from)
                .ok_or_else(|| Error(format!("unknown service {port}")))?
        }
    };
    let rw = |p: &str| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(p)
            .map_err(|e| Error(format!("can't open {p}: {}", Error::from(e).0)))
    };
    let mut ctl = rw(&format!("{netdir}/{proto}/clone"))?;
    let mut idbuf = [0u8; 32];
    let k = ctl.read(&mut idbuf)?;
    let id = String::from_utf8_lossy(&idbuf[..k]).trim().to_string();
    ctl.write_all(format!("announce {host}!{port}").as_bytes())
        .map_err(Error::from)?;
    ctl.write_all(b"headers")
        .map_err(|e| Error(format!("can't set udp headers: {}", Error::from(e).0)))?;
    let dir = format!("{netdir}/{proto}/{id}");
    let data = rw(&format!("{dir}/data"))?;
    let local = std::fs::read_to_string(format!("{dir}/local"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Ok((ctl, data, local))
}

impl Rip {
    fn ismyaddr(&self, a: u32) -> bool {
        self.nets.iter().any(|n| n.ip == a)
    }

    /// The route to `dest`, or a free slot for one.
    fn lookup(&mut self, dest: u32) -> usize {
        if let Some(i) = self.routes.iter().position(|g| g.valid && g.dest == dest) {
            return i;
        }
        let i = match self.routes.iter().position(|g| !g.valid) {
            Some(i) => i,
            None => {
                self.routes.push(Gateway::default());
                self.routes.len() - 1
            }
        };
        self.routes[i].dest = dest;
        i
    }

    fn addlocal(&mut self) {
        for ifc in self.nets.clone() {
            let i = self.lookup(ifc.net);
            let g = &mut self.routes[i];
            g.valid = true;
            g.local = true;
            g.gateway = ifc.ip;
            g.mask = ifc.mask;
            g.metric = NETWORK_COST;
            g.time = 0;
            g.changed = true;
            if self.debug > 0 {
                syslog(&format!(
                    "Existing: {} & {} -> {}",
                    text(g.dest),
                    text(g.mask),
                    text(g.gateway)
                ));
            }
        }
    }

    /// Notes the routes already in `iproute`, but not the interfaces'
    /// own or broadcast, unicast-to-self and multicast ones.
    fn readroutes(&mut self, table: &str) {
        for l in table.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 5 {
                continue;
            }
            let flags = f[3];
            if !flags.starts_with('4') || flags.contains(['i', 'b', 'u', 'm']) {
                continue;
            }
            let (Some(dest), Some(mask), Some(gw)) =
                (parse_ip(f[0]), parse_ip(f[1]), parse_ip(f[2]))
            else {
                continue;
            };
            let i = self.lookup(dest);
            let defgw = self.defroute.map(|d| self.routes[d].gateway);
            let mine = self.ismyaddr(gw);
            let g = &mut self.routes[i];
            g.valid = true;
            g.mask = mask;
            g.gateway = gw;
            g.metric = HOP_LIMIT;
            g.time = self.now;
            g.changed = true;
            if self.debug > 0 {
                syslog(&format!(
                    "Existing: {} & {} -> {}",
                    text(dest),
                    text(mask),
                    text(gw)
                ));
            }
            if dest == 0 && mask == 0 {
                g.local = true;
                self.defroute = Some(i);
            } else if defgw == Some(dest) {
                continue;
            } else {
                g.local = !mine;
            }
        }
    }

    fn unpackroute(&mut self, b: &[u8], version: u8, from: u32) {
        let get4 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        if b[0] != 0 || u16::from(b[1]) != AF_INET {
            if self.debug > 1 {
                eprintln!("\t-- unknown address type {:x},{:x}", b[0], b[1]);
            }
            return;
        }
        let dest = get4(4);
        let mut gw = from;
        let mask = if version == 1 {
            if u16::from_be_bytes([b[2], b[3]]) != 0 || get4(8) != 0 || get4(12) != 0 {
                if self.debug > 1 {
                    eprintln!("\t-- non-zero MBZ");
                }
                return;
            }
            self.maskgen(dest)
        } else {
            if get4(12) != 0 {
                gw = get4(12);
            }
            match get4(8) {
                0 => self.maskgen(dest),
                m => m,
            }
        };
        let metric = get4(16);
        if self.debug > 1 {
            eprintln!("\t{} {metric}", text(dest));
        }
        if metric == 0 || metric > HOP_LIMIT {
            return;
        }
        let top = dest >> 24;
        if (224..256).contains(&top) || top == 0 || top == 0x7f {
            if self.debug > 1 {
                eprintln!("\t{} {metric} invalid addr", text(dest));
            }
            return;
        }
        if dest & !mask == !mask {
            if self.debug > 1 {
                eprintln!("\t{} & {} -> broadcast", text(dest), text(mask));
            }
            return;
        }
        let metric = (metric + NETWORK_COST).min(HOP_LIMIT);
        self.updateroute(dest, mask, gw, metric);
    }

    /// RFC 1058's rules for a response (pages 27 and 28), replacing a
    /// route that is about to expire with one as good.
    fn updateroute(&mut self, dest: u32, mask: u32, gw: u32, metric: u32) {
        let i = self.lookup(dest);
        let now = self.now;
        let r = self.routes[i];
        if r.valid {
            if r.local {
                return;
            }
            if r.gateway == gw {
                if metric != HOP_LIMIT {
                    let r = &mut self.routes[i];
                    r.metric = metric;
                    r.time = now;
                } else if r.metric != HOP_LIMIT {
                    let r = &mut self.routes[i];
                    r.metric = metric;
                    r.changed = true;
                    r.time = now.saturating_sub(DISCARD - 120);
                    self.delroute(i);
                } else if now >= r.time + DISCARD {
                    self.delroute(i);
                    self.routes[i].valid = false;
                    self.routes[i].changed = true;
                }
            } else if metric < r.metric
                || (metric != HOP_LIMIT && metric == r.metric && now > r.time + EXPIRED / 2)
            {
                self.delroute(i);
                let r = &mut self.routes[i];
                r.metric = metric;
                r.gateway = gw;
                r.time = now;
                self.addroute(i);
            }
        } else if metric < HOP_LIMIT {
            let better = self
                .findroute(dest)
                .is_none_or(|p| metric <= self.routes[p].metric);
            if mask != u32::MAX || better {
                let r = &mut self.routes[i];
                r.changed = true;
                r.time = now;
                r.metric = metric;
                r.dest = dest;
                r.mask = mask;
                r.gateway = gw;
                r.local = false;
                self.addroute(i);
            }
        }
    }

    /// The valid route with the most specific mask that holds `a`.
    fn findroute(&self, a: u32) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, r) in self.routes.iter().enumerate() {
            if !r.valid || a & r.mask != r.dest {
                continue;
            }
            if best.is_none_or(|b| {
                let pm = self.routes[b].mask;
                r.mask & pm != r.mask
            }) {
                best = Some(i);
            }
        }
        best
    }

    /// The mask of the least specific interface network that holds
    /// `addr`, else its class's.
    fn maskgen(&self, addr: u32) -> u32 {
        let mut best: Option<Ifc> = None;
        for n in &self.nets {
            if addr & n.mask == n.net && best.is_none_or(|b| n.mask & b.mask == n.mask) {
                best = Some(*n);
            }
        }
        best.map_or_else(|| class_mask(addr), |n| n.mask)
    }

    fn addroute(&mut self, i: usize) {
        let g = self.routes[i];
        let defgw = self.defroute.map(|d| self.routes[d].gateway);
        if g.mask == 0 && g.dest == 0 {
            // The default route is not changed.
            self.routes[i].valid = false;
        } else if defgw == Some(g.gateway) {
            if self.debug > 0 {
                syslog(&format!("default {} {}", text(g.dest), text(g.mask)));
            }
            self.routes[i].valid = true;
            self.routes[i].changed = true;
        } else {
            let cmd = format!("add {} {} {}", text(g.dest), text(g.mask), text(g.gateway));
            if self.debug > 0 {
                syslog(&cmd);
            }
            let ok = self.nochange || self.write_route(&cmd);
            self.routes[i].valid = ok;
            if ok {
                self.routes[i].changed = true;
            }
        }
    }

    fn delroute(&mut self, i: usize) {
        let g = self.routes[i];
        if self.debug > 0 {
            syslog(&format!("delete {} {}", text(g.dest), text(g.mask)));
        }
        if !self.nochange {
            self.write_route(&format!("remove {} {}", text(g.dest), text(g.mask)));
        }
    }

    fn write_route(&mut self, cmd: &str) -> bool {
        let Some(f) = self.route.as_mut() else {
            return false;
        };
        match f.write_all(cmd.as_bytes()) {
            Ok(()) => true,
            Err(e) => {
                if self.debug > 0 {
                    syslog(&format!("can't {cmd}: {}", Error::from(e)));
                }
                false
            }
        }
    }

    /// Sends a request or the (changed) routes to each interface's
    /// broadcast address.
    fn sendall(&mut self, data: &mut File, op: u8, changes: bool) {
        for ifc in self.nets.clone() {
            if !self.only.is_empty() && !self.only.contains(&ifc.net) {
                continue;
            }
            let dst = ifc.net | !ifc.mask;
            self.sendroutes(data, &ifc, dst, op, changes);
        }
        for r in &mut self.routes {
            r.changed = false;
        }
    }

    fn sendroutes(&self, data: &mut File, ifc: &Ifc, dst: u32, op: u8, changes: bool) {
        if self.debug > 1 {
            syslog(&format!("send {}", text(dst)));
        }
        let mut hdr = [0u8; UDP_HEADER];
        hdr[10] = 0xff;
        hdr[11] = 0xff;
        hdr[12..16].copy_from_slice(&dst.to_be_bytes());
        hdr[48..50].copy_from_slice(&RIP_PORT.to_be_bytes());
        hdr[50..52].copy_from_slice(&RIP_PORT.to_be_bytes());
        let start = || {
            let mut b = hdr.to_vec();
            b.extend_from_slice(&[op, self.version, 0, 0]);
            b
        };
        let mut buf = start();
        let send = |buf: &[u8], data: &mut File| {
            if data.write_all(buf).is_err() {
                eprintln!(
                    "rip: network write to {} failed: {}",
                    text(dst),
                    purgatorio::errstr()
                );
            }
        };
        if op == OP_REQUEST {
            let mut e = [0u8; ENTRY];
            e[16..20].copy_from_slice(&HOP_LIMIT.to_be_bytes());
            buf.extend_from_slice(&e);
        } else {
            for (i, r) in self.routes.iter().enumerate() {
                if !r.valid || (changes && !r.changed) || Some(i) == self.defroute {
                    continue;
                }
                if r.dest == ifc.net || r.dest & ifc.mask == ifc.net {
                    continue;
                }
                let netmask = class_mask(r.dest);
                let subnet = r.mask != netmask;
                if self.version < 2 && r.mask != u32::MAX && subnet && netmask != class_mask(ifc.ip)
                {
                    continue;
                }
                if buf.len() + ENTRY > UDP_HEADER + RIP_HEADER + MAX_RIP {
                    send(&buf, data);
                    buf = start();
                }
                let mut e = [0u8; ENTRY];
                e[0..2].copy_from_slice(&AF_INET.to_be_bytes());
                e[4..8].copy_from_slice(&r.dest.to_be_bytes());
                if self.version == 2 && subnet {
                    e[8..12].copy_from_slice(&r.mask.to_be_bytes());
                }
                e[16..20].copy_from_slice(&r.metric.to_be_bytes());
                buf.extend_from_slice(&e);
            }
        }
        if buf.len() > UDP_HEADER + RIP_HEADER {
            send(&buf, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rip() -> Rip {
        Rip {
            debug: 0,
            nochange: true,
            version: 1,
            nets: vec![Ifc {
                ip: parse_ip("10.0.0.5").unwrap(),
                mask: 0xffff_ff00,
                net: parse_ip("10.0.0.0").unwrap(),
            }],
            only: Vec::new(),
            routes: Vec::new(),
            defroute: None,
            route: None,
            now: 1000,
        }
    }

    fn entry(dest: &str, mask: u32, hop: u32, metric: u32) -> [u8; ENTRY] {
        let mut e = [0u8; ENTRY];
        e[1] = 2;
        e[4..8].copy_from_slice(&parse_ip(dest).unwrap().to_be_bytes());
        e[8..12].copy_from_slice(&mask.to_be_bytes());
        e[12..16].copy_from_slice(&hop.to_be_bytes());
        e[16..20].copy_from_slice(&metric.to_be_bytes());
        e
    }

    #[test]
    fn learns_and_replaces() {
        let mut r = rip();
        r.addlocal();
        let gw1 = parse_ip("10.0.0.1").unwrap();
        let gw2 = parse_ip("10.0.0.2").unwrap();
        r.unpackroute(&entry("192.0.2.0", 0, 0, 3), 1, gw1);
        let i = r.findroute(parse_ip("192.0.2.7").unwrap()).unwrap();
        assert_eq!((r.routes[i].gateway, r.routes[i].metric), (gw1, 4));
        assert_eq!(r.routes[i].mask, 0xffff_ff00);
        // A better metric from another gateway wins; a worse one does not.
        r.unpackroute(&entry("192.0.2.0", 0, 0, 1), 1, gw2);
        assert_eq!(r.routes[i].gateway, gw2);
        r.unpackroute(&entry("192.0.2.0", 0, 0, 5), 1, gw1);
        assert_eq!(r.routes[i].gateway, gw2);
        // Version 1 needs zero mask and next hop; version 2 uses them.
        r.unpackroute(&entry("198.51.100.0", 0xffff_ff80, 0, 1), 1, gw1);
        assert!(r.findroute(parse_ip("198.51.100.1").unwrap()).is_none());
        r.unpackroute(&entry("198.51.100.0", 0xffff_ff80, gw2, 1), 2, gw1);
        let j = r.findroute(parse_ip("198.51.100.1").unwrap()).unwrap();
        assert_eq!((r.routes[j].gateway, r.routes[j].mask), (gw2, 0xffff_ff80));
        // Unreachable from its own gateway: withdrawn.
        r.unpackroute(&entry("198.51.100.0", 0xffff_ff80, gw2, 16), 2, gw1);
        assert_eq!(r.routes[j].metric, HOP_LIMIT);
        // Loopback, multicast and our own network are ignored.
        r.unpackroute(&entry("127.0.0.0", 0, 0, 1), 1, gw1);
        r.unpackroute(&entry("224.0.0.0", 0, 0, 1), 1, gw1);
        assert!(r.findroute(parse_ip("127.0.0.1").unwrap()).is_none());
        r.unpackroute(&entry("10.0.0.0", 0, 0, 1), 1, gw1);
        let k = r.findroute(parse_ip("10.0.0.9").unwrap()).unwrap();
        assert!(r.routes[k].local);
    }
}
