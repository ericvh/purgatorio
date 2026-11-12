//! `#I`, the IP device. It serves `/net/tcp`, `/net/udp`, `/net/icmp`,
//! `/net/ipifc`, `/net/iproute` and `/net/cs` the way Inferno's `devip` and
//! connection server do, and asks the platform's
//! [`purg_platform::net::Net`] to open the sockets.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use purg_abi::{error, mode};
use purg_platform::net::{Conn, IPV4_HEADER, Ifc, Listener, Net};
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::Platform;
use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QROOT: u64 = 0;
const QTCP: u64 = 1;
const QUDP: u64 = 2;
const QCS: u64 = 3;
const QICMP: u64 = 4;
const QIPIFC: u64 = 5;
const QIPROUTE: u64 = 6;

const PROTO_TCP: u8 = 1;
const PROTO_UDP: u8 = 2;
const PROTO_ICMP: u8 = 3;
/// Not a protocol: the paths of `/net/ipifc/n` and its files.
const IFC: u8 = 0x10;

/// Every protocol directory, in the order `#I` lists them.
const PROTOS: [(u8, u64, &str); 3] = [
    (PROTO_TCP, QTCP, "tcp"),
    (PROTO_UDP, QUDP, "udp"),
    (PROTO_ICMP, QICMP, "icmp"),
];

const FDIR: u8 = 0;
const FCTL: u8 = 1;
const FDATA: u8 = 2;
const FLISTEN: u8 = 3;
const FLOCAL: u8 = 4;
const FREMOTE: u8 = 5;
const FSTATUS: u8 = 6;

fn qconv(proto: u8, id: u32, file: u8) -> u64 {
    (u64::from(proto) << 40) | (u64::from(id) << 8) | u64::from(file)
}

fn is_conv(path: u64) -> bool {
    path >> 40 != 0 && conv_proto(path) != IFC
}

fn conv_proto(path: u64) -> u8 {
    ((path >> 40) & 0xff) as u8
}

fn conv_id(path: u64) -> u32 {
    ((path >> 8) & 0xffff_ffff) as u32
}

fn conv_file(path: u64) -> u8 {
    (path & 0xff) as u8
}

/// The clone file uses an id no conversation will ever have.
fn qclone(proto: u8) -> u64 {
    qconv(proto, 0xffff_ffff, FCTL)
}

struct Conv {
    id: u32,
    proto: u8,
    conn: Mutex<Option<Arc<dyn Conn>>>,
    listener: Mutex<Option<Arc<dyn Listener>>>,
    local: Mutex<String>,
    remote: Mutex<String>,
    status: Mutex<String>,
    /// `headers` was written: an announced UDP port is read and written
    /// whole on this conversation, each datagram after a header.
    headers: AtomicBool,
    /// The host an announce named, for re-announcing in `headers` mode.
    announced: Mutex<String>,
    /// Open Chans on the conversation's files. When the last one closes
    /// the conversation is hung up and forgotten, as in Inferno.
    opens: AtomicU32,
}

impl Conv {
    fn fresh(id: u32, proto: u8) -> Conv {
        Conv {
            id,
            proto,
            conn: Mutex::new(None),
            listener: Mutex::new(None),
            local: Mutex::new(String::new()),
            remote: Mutex::new(String::new()),
            status: Mutex::new(String::from("Init")),
            headers: AtomicBool::new(false),
            announced: Mutex::new(String::new()),
            opens: AtomicU32::new(0),
        }
    }
}

/// `#I`.
pub struct DevIp {
    platform: Arc<dyn Platform>,
    convs: Mutex<Vec<Arc<Conv>>>,
    next: Mutex<u32>,
}

impl DevIp {
    pub fn new(platform: Arc<dyn Platform>) -> DevIp {
        DevIp {
            platform,
            convs: Mutex::new(Vec::new()),
            next: Mutex::new(0),
        }
    }

    fn net(&self) -> Result<&dyn Net> {
        self.platform.net().ok_or(Error::new("network unreachable"))
    }

    fn alloc(&self, proto: u8) -> Arc<Conv> {
        let mut n = self.next.lock();
        let id = *n;
        *n += 1;
        drop(n);
        let c = Arc::new(Conv::fresh(id, proto));
        self.convs.lock().push(Arc::clone(&c));
        c
    }

    fn find(&self, proto: u8, id: u32) -> Option<Arc<Conv>> {
        self.convs
            .lock()
            .iter()
            .find(|c| c.proto == proto && c.id == id)
            .cloned()
    }

    fn qid(path: u64) -> Qid {
        let qtype = match path {
            QROOT | QTCP | QUDP | QICMP | QIPIFC => QTDIR,
            q if is_ifc(q) && conv_file(q) == FDIR => QTDIR,
            q if is_conv(q) && conv_file(q) == FDIR => QTDIR,
            _ => QTFILE,
        };
        Qid {
            path,
            vers: 0,
            qtype,
        }
    }

    fn dirent(&self, path: u64) -> Dir {
        let (name, _qtype, perm) = match path {
            QROOT => (String::from("#I"), QTDIR, DMDIR | 0o555),
            QTCP => (String::from("tcp"), QTDIR, DMDIR | 0o555),
            QUDP => (String::from("udp"), QTDIR, DMDIR | 0o555),
            QICMP => (String::from("icmp"), QTDIR, DMDIR | 0o555),
            QCS => (String::from("cs"), QTFILE, 0o666),
            QIPIFC => (String::from("ipifc"), QTDIR, DMDIR | 0o555),
            QIPROUTE => (String::from("iproute"), QTFILE, 0o666),
            q if is_ifc(q) => match conv_file(q) {
                FDIR => (conv_id(q).to_string(), QTDIR, DMDIR | 0o555),
                FCTL => (String::from("ctl"), QTFILE, 0o666),
                FLOCAL => (String::from("local"), QTFILE, 0o444),
                _ => (String::from("status"), QTFILE, 0o444),
            },
            q if clone_proto(q).is_some() => (String::from("clone"), QTFILE, 0o666),
            q if is_conv(q) => match conv_file(q) {
                FDIR => (conv_id(q).to_string(), QTDIR, DMDIR | 0o555),
                FCTL => (String::from("ctl"), QTFILE, 0o666),
                FDATA => (String::from("data"), QTFILE, 0o666),
                FLISTEN => (String::from("listen"), QTFILE, 0o666),
                FLOCAL => (String::from("local"), QTFILE, 0o444),
                FREMOTE => (String::from("remote"), QTFILE, 0o444),
                FSTATUS => (String::from("status"), QTFILE, 0o444),
                _ => (String::from("?"), QTFILE, 0o444),
            },
            _ => (String::from("?"), QTFILE, 0o444),
        };
        let user = self.platform.user();
        Dir {
            dtype: u16::from(b'I'),
            qid: Self::qid(path),
            mode: perm,
            name,
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        }
    }
}

impl Dev for DevIp {
    fn dc(&self) -> char {
        'I'
    }

    fn name(&self) -> &'static str {
        "ip"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Self::qid(QROOT),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let next = match c.qid.path {
            QROOT => match name {
                "tcp" => QTCP,
                "udp" => QUDP,
                "icmp" => QICMP,
                "cs" => QCS,
                "ipifc" => QIPIFC,
                "iproute" => QIPROUTE,
                _ => return Err(Error::new(error::ENONEXIST)),
            },
            QTCP => proto_child(self, PROTO_TCP, name)?,
            QUDP => proto_child(self, PROTO_UDP, name)?,
            QICMP => proto_child(self, PROTO_ICMP, name)?,
            QIPIFC => {
                let n: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                if n as usize >= self.interfaces().len() || name != n.to_string() {
                    return Err(Error::new(error::ENONEXIST));
                }
                qconv(IFC, n, FDIR)
            }
            q if is_ifc(q) && conv_file(q) == FDIR => {
                let file = match name {
                    "ctl" => FCTL,
                    "local" => FLOCAL,
                    "status" => FSTATUS,
                    _ => return Err(Error::new(error::ENONEXIST)),
                };
                qconv(IFC, conv_id(q), file)
            }
            q if is_conv(q) && conv_file(q) == FDIR => file_child(q, name)?,
            _ => return Err(Error::new(error::ENOTDIR)),
        };
        Ok(Walked {
            qid: Self::qid(next),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dirent(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.qtype == QTDIR && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EISDIR));
        }
        if c.qid.path == QCS {
            c.state.lock().replace(Box::new(String::new()));
            return Ok(());
        }
        if let Some(proto) = clone_proto(c.qid.path) {
            let conv = self.alloc(proto);
            c.qid = Self::qid(qconv(proto, conv.id, FCTL));
        }
        if is_conv(c.qid.path) {
            let conv = self
                .find(conv_proto(c.qid.path), conv_id(c.qid.path))
                .ok_or(Error::new(error::ENONEXIST))?;
            conv.opens.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    /// The last close of a conversation's files hangs it up and removes
    /// it, so the peer sees the end and the platform's socket is freed. A
    /// conversation a listener returned stays until it has been opened
    /// and closed.
    fn close(&self, c: &Chan) {
        if !is_conv(c.qid.path) {
            return;
        }
        let (proto, id) = (conv_proto(c.qid.path), conv_id(c.qid.path));
        let gone = {
            let mut convs = self.convs.lock();
            let Some(i) = convs.iter().position(|v| v.proto == proto && v.id == id) else {
                return;
            };
            if convs[i].opens.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            convs.remove(i)
        };
        // Outside the table lock: hanging up may block in the platform.
        if let Some(conn) = gone.conn.lock().take() {
            conn.hangup();
        }
        gone.listener.lock().take();
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path == QCS {
            let text = c
                .state
                .lock()
                .as_ref()
                .and_then(|s| s.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            return Ok(read_str(text.as_bytes(), buf, offset));
        }
        if c.qid.path == QIPROUTE {
            return Ok(read_str(self.routes().as_bytes(), buf, offset));
        }
        if is_ifc(c.qid.path) {
            let n = conv_id(c.qid.path);
            let text = match self.interfaces().get(n as usize) {
                None => return Err(Error::new(error::ENONEXIST)),
                Some(i) => match conv_file(c.qid.path) {
                    FCTL => format!("{n}\n"),
                    FLOCAL => ifc_local(i),
                    FSTATUS => ifc_status(i),
                    _ => return Err(Error::new(error::EISDIR)),
                },
            };
            return Ok(read_str(text.as_bytes(), buf, offset));
        }
        if !is_conv(c.qid.path) {
            return Err(Error::new(error::EISDIR));
        }
        let conv = self
            .find(conv_proto(c.qid.path), conv_id(c.qid.path))
            .ok_or(Error::new(error::ENONEXIST))?;
        match conv_file(c.qid.path) {
            FCTL => Ok(read_str(format!("{}\n", conv.id).as_bytes(), buf, offset)),
            FDATA => {
                let conn = conv
                    .conn
                    .lock()
                    .clone()
                    .ok_or(Error::new("not connected"))?;
                conn.read(buf).map_err(Error::from)
            }
            FLISTEN => accept(self, &conv, buf),
            FLOCAL => Ok(read_str(
                format!("{}\n", conv.local.lock().clone()).as_bytes(),
                buf,
                offset,
            )),
            FREMOTE => Ok(read_str(
                format!("{}\n", conv.remote.lock().clone()).as_bytes(),
                buf,
                offset,
            )),
            FSTATUS => Ok(read_str(
                format!("{}\n", conv.status.lock().clone()).as_bytes(),
                buf,
                offset,
            )),
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let mut entries = Vec::new();
        match c.qid.path {
            QROOT => {
                for (_, dir, _) in PROTOS {
                    entries.push(self.dirent(dir));
                }
                entries.push(self.dirent(QCS));
                entries.push(self.dirent(QIPIFC));
                entries.push(self.dirent(QIPROUTE));
            }
            QIPIFC => {
                for n in 0..self.interfaces().len() {
                    entries.push(self.dirent(qconv(IFC, n as u32, FDIR)));
                }
            }
            q if is_ifc(q) && conv_file(q) == FDIR => {
                for f in [FCTL, FLOCAL, FSTATUS] {
                    entries.push(self.dirent(qconv(IFC, conv_id(q), f)));
                }
            }
            QTCP => self.push_proto(&mut entries, PROTO_TCP),
            QUDP => self.push_proto(&mut entries, PROTO_UDP),
            QICMP => self.push_proto(&mut entries, PROTO_ICMP),
            q if is_conv(q) && conv_file(q) == FDIR => {
                for f in [FCTL, FDATA, FLISTEN, FLOCAL, FREMOTE, FSTATUS] {
                    entries.push(self.dirent(qconv(conv_proto(q), conv_id(q), f)));
                }
            }
            _ => return Err(Error::new(error::ENOTDIR)),
        }
        Ok(entries)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        if c.qid.path == QCS {
            if offset != 0 {
                return Err(Error::new("bad offset"));
            }
            let text = core::str::from_utf8(buf).unwrap_or("").trim().to_string();
            let translated = self.translate(&text)?;
            c.state.lock().replace(Box::new(translated));
            return Ok(buf.len());
        }
        if c.qid.path == QIPROUTE {
            let cmd = core::str::from_utf8(buf).unwrap_or("").trim();
            self.route_ctl(cmd)?;
            return Ok(buf.len());
        }
        if is_ifc(c.qid.path) && conv_file(c.qid.path) == FCTL {
            let cmd = core::str::from_utf8(buf).unwrap_or("").trim();
            self.ifc_ctl(conv_id(c.qid.path) as usize, cmd)?;
            return Ok(buf.len());
        }
        if !is_conv(c.qid.path) {
            return Err(Error::new(error::EPERM));
        }
        let conv = self
            .find(conv_proto(c.qid.path), conv_id(c.qid.path))
            .ok_or(Error::new(error::ENONEXIST))?;
        match conv_file(c.qid.path) {
            FCTL => {
                let cmd = core::str::from_utf8(buf).unwrap_or("").trim().to_string();
                self.ctl(&conv, &cmd)?;
                Ok(buf.len())
            }
            FDATA => {
                let conn = conv
                    .conn
                    .lock()
                    .clone()
                    .ok_or(Error::new("not connected"))?;
                if conv.proto == PROTO_ICMP {
                    // Inferno's icmp writes start with room for an IPv4
                    // header, which the stack fills in; it drops writes
                    // too short to hold an ICMP header after it.
                    if buf.len() < IPV4_HEADER + 8 {
                        return Ok(buf.len());
                    }
                    conn.write(&buf[IPV4_HEADER..]).map_err(Error::from)?;
                    return Ok(buf.len());
                }
                conn.write(buf).map_err(Error::from)
            }
            _ => Err(Error::new(error::EPERM)),
        }
    }
}

impl DevIp {
    fn push_proto(&self, entries: &mut Vec<Dir>, proto: u8) {
        entries.push(self.dirent(qclone(proto)));
        for c in self.convs.lock().iter().filter(|c| c.proto == proto) {
            entries.push(self.dirent(qconv(proto, c.id, FDIR)));
        }
    }

    fn ctl(&self, conv: &Conv, cmd: &str) -> Result<()> {
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let op = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        match op {
            "connect" => self.connect(conv, rest),
            "announce" => self.announce(conv, rest),
            "headers" => self.headers(conv),
            "hangup" => {
                if let Some(conn) = conv.conn.lock().clone() {
                    conn.hangup();
                }
                *conv.status.lock() = String::from("Closed");
                Ok(())
            }
            _ => Err(Error::new(error::EBADARG)),
        }
    }

    fn connect(&self, conv: &Conv, addr: &str) -> Result<()> {
        let net = self.net()?;
        if conv.proto == PROTO_ICMP {
            // `connect host!1`: ICMP has no ports; the number is ignored.
            let host = addr.split('!').next().unwrap_or("").trim();
            let host = host.trim_matches(|c| c == '[' || c == ']');
            let conn = net.dial_icmp(host).map_err(Error::from)?;
            *conv.local.lock() = conn.local();
            *conv.remote.lock() = conn.remote();
            *conv.status.lock() = String::from("Established");
            *conv.conn.lock() = Some(conn);
            return Ok(());
        }
        let (host, port) = host_port(addr)?;
        let conn = if conv.proto == PROTO_UDP {
            net.dial_udp(&host, port).map_err(Error::from)?
        } else {
            net.dial_tcp(&host, port).map_err(Error::from)?
        };
        *conv.local.lock() = conn.local();
        *conv.remote.lock() = conn.remote();
        *conv.status.lock() = String::from("Established");
        *conv.conn.lock() = Some(conn);
        Ok(())
    }

    fn announce(&self, conv: &Conv, addr: &str) -> Result<()> {
        if conv.proto == PROTO_ICMP {
            return Err(Error::new("protocol not supported"));
        }
        let (host, port) = host_port(addr)?;
        let host = if host.is_empty() {
            String::from("*")
        } else {
            host
        };
        let net = self.net()?;
        *conv.announced.lock() = host.clone();
        if conv.proto == PROTO_UDP && conv.headers.load(Ordering::Acquire) {
            return self.announce_headers(conv, &host, port);
        }
        // An announced UDP port is listened on like TCP: each new remote is
        // a new conversation.
        let listener = if conv.proto == PROTO_UDP {
            net.announce_udp(&host, port)
        } else {
            net.announce_tcp(&host, port)
        }
        .map_err(Error::from)?;
        *conv.local.lock() = listener.local();
        *conv.status.lock() = String::from("Listen");
        *conv.listener.lock() = Some(listener);
        Ok(())
    }

    /// `headers` on a UDP conversation. On one already announced, the
    /// port is announced again in headers mode: its `listen` file stops
    /// returning conversations and its `data` file has every datagram.
    fn headers(&self, conv: &Conv) -> Result<()> {
        if conv.proto != PROTO_UDP {
            return Err(Error::new(error::EBADARG));
        }
        if conv.headers.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Some(listener) = conv.listener.lock().take() else {
            return Ok(());
        };
        let local = listener.local();
        let port: u16 = local
            .rsplit('!')
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or(Error::new("bad port"))?;
        // The port is free again once its listener is gone.
        drop(listener);
        let host = conv.announced.lock().clone();
        self.announce_headers(conv, &host, port)
    }

    fn announce_headers(&self, conv: &Conv, host: &str, port: u16) -> Result<()> {
        let conn = self
            .net()?
            .announce_udp_headers(host, port)
            .map_err(Error::from)?;
        *conv.local.lock() = conn.local();
        *conv.status.lock() = String::from("Announced");
        *conv.conn.lock() = Some(conn);
        Ok(())
    }

    /// `cs`: `tcp!host!port` becomes `tcp!ip!port`.
    fn translate(&self, query: &str) -> Result<String> {
        let (net, host, port) = parse_addr(query);
        if host == "*" || host.is_empty() || looks_numeric(&host) {
            return Ok(format!("{net}!{host}!{port}\n"));
        }
        let p: u16 = port.parse().map_err(|_| Error::new("bad port"))?;
        let ip = self.net()?.resolve(&host, p).map_err(Error::from)?;
        Ok(format!("{net}!{ip}!{port}\n"))
    }
}

impl DevIp {
    fn interfaces(&self) -> Vec<Ifc> {
        self.platform
            .net()
            .map(|n| n.interfaces())
            .unwrap_or_default()
    }

    /// `/net/iproute`: each interface's network, then the default route.
    fn routes(&self) -> String {
        let mut out = String::new();
        for (n, i) in self.interfaces().iter().enumerate() {
            let mask = prefix_mask(i.prefix);
            out.push_str(&format!(
                "{} {} {} 4i none {n}\n",
                ip(network(i.addr, mask)),
                ip(mask),
                ip(i.addr)
            ));
        }
        for (n, i) in self.interfaces().iter().enumerate() {
            if let Some(gw) = i.gateway {
                out.push_str(&format!("0.0.0.0 0.0.0.0 {} 4u none {n}\n", ip(gw)));
            }
        }
        out
    }

    /// `add 0.0.0.0 0.0.0.0 gw` and `remove 0.0.0.0 0.0.0.0`: only the
    /// default route can be changed.
    fn route_ctl(&self, cmd: &str) -> Result<()> {
        let f: Vec<&str> = cmd.split_whitespace().collect();
        let default = |a: &str, m: &str| parse_ip(a) == Some([0; 4]) && parse_ip(m) == Some([0; 4]);
        match f.as_slice() {
            ["add", a, m, gw, ..] if default(a, m) => {
                let gw = parse_ip(gw).ok_or(Error::new(error::EBADARG))?;
                self.net()?.set_gateway(Some(gw)).map_err(Error::from)
            }
            ["remove", a, m, ..] if default(a, m) => {
                self.net()?.set_gateway(None).map_err(Error::from)
            }
            ["flush", ..] => self.net()?.set_gateway(None).map_err(Error::from),
            _ => Err(Error::new(error::EBADARG)),
        }
    }

    /// `add ip [mask]` and `connect ip [mask]`: the interface's address.
    fn ifc_ctl(&self, n: usize, cmd: &str) -> Result<()> {
        let f: Vec<&str> = cmd.split_whitespace().collect();
        let (addr, mask) = match f.as_slice() {
            ["add" | "connect", a] => (parse_ip(a), None),
            ["add" | "connect", a, m, ..] => (parse_ip(a), Some(parse_ip(m))),
            _ => return Err(Error::new(error::EBADARG)),
        };
        let addr = addr.ok_or(Error::new(error::EBADARG))?;
        let mask = match mask {
            None => class_mask(addr),
            Some(m) => m.ok_or(Error::new(error::EBADARG))?,
        };
        let prefix = u32::from_be_bytes(mask).leading_ones() as u8;
        if prefix_mask(prefix) != mask {
            return Err(Error::new(error::EBADARG));
        }
        self.net()?
            .set_address(n, addr, prefix)
            .map_err(Error::from)
    }
}

/// `ipifc/n/local`: the address, then the addresses that reach it (the
/// network and its broadcast).
fn ifc_local(i: &Ifc) -> String {
    let mask = prefix_mask(i.prefix);
    let net = network(i.addr, mask);
    let bcast = u32::from_be_bytes(net) | !u32::from_be_bytes(mask);
    format!(
        "{} -> {} {}\n",
        ip(i.addr),
        ip(bcast.to_be_bytes()),
        ip(net)
    )
}

/// `ipifc/n/status`: the device and MTU, and (Purgatorio's addition, for
/// `ip/dhcp`) the hardware address; then the address, mask and network,
/// and packet counts, which are not kept.
fn ifc_status(i: &Ifc) -> String {
    let mac: String = i.mac.iter().map(|b| format!("{b:02x}")).collect();
    let mask = prefix_mask(i.prefix);
    format!(
        "{} {} {mac}\n{} {} {} 0 0 0 0\n",
        i.device,
        i.mtu,
        ip(i.addr),
        ip(mask),
        ip(network(i.addr, mask))
    )
}

fn prefix_mask(prefix: u8) -> [u8; 4] {
    let bits = u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0);
    bits.to_be_bytes()
}

fn network(addr: [u8; 4], mask: [u8; 4]) -> [u8; 4] {
    (u32::from_be_bytes(addr) & u32::from_be_bytes(mask)).to_be_bytes()
}

fn class_mask(a: [u8; 4]) -> [u8; 4] {
    match a[0] {
        0..=127 => [255, 0, 0, 0],
        128..=191 => [255, 255, 0, 0],
        _ => [255, 255, 255, 0],
    }
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut out {
        *b = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn ip(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn is_ifc(path: u64) -> bool {
    conv_proto(path) == IFC
}

/// The protocol whose clone file `path` is, if it is one.
fn clone_proto(path: u64) -> Option<u8> {
    PROTOS
        .iter()
        .map(|(p, _, _)| *p)
        .find(|&p| path == qclone(p))
}

fn proto_child(dev: &DevIp, proto: u8, name: &str) -> Result<u64> {
    if name == "clone" {
        return Ok(qclone(proto));
    }
    let id: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
    let conv = dev.find(proto, id).ok_or(Error::new(error::ENONEXIST))?;
    Ok(qconv(conv.proto, conv.id, FDIR))
}

fn file_child(dir: u64, name: &str) -> Result<u64> {
    let file = match name {
        "ctl" => FCTL,
        "data" => FDATA,
        "listen" => FLISTEN,
        "local" => FLOCAL,
        "remote" => FREMOTE,
        "status" => FSTATUS,
        _ => return Err(Error::new(error::ENONEXIST)),
    };
    Ok(qconv(conv_proto(dir), conv_id(dir), file))
}

/// One read accepts one connection and returns its directory number.
fn accept(dev: &DevIp, conv: &Conv, buf: &mut [u8]) -> Result<usize> {
    let listener = conv
        .listener
        .lock()
        .clone()
        .ok_or(Error::new("not announced"))?;
    let conn = listener.accept().map_err(Error::from)?;
    let born = dev.alloc(conv.proto);
    *born.local.lock() = conn.local();
    *born.remote.lock() = conn.remote();
    *born.status.lock() = String::from("Established");
    *born.conn.lock() = Some(conn);
    let text = format!("{}\n", born.id);
    let n = text.len().min(buf.len());
    buf[..n].copy_from_slice(&text.as_bytes()[..n]);
    Ok(n)
}

/// `tcp!host!port`, `host!port` or a bare port.
fn parse_addr(s: &str) -> (String, String, String) {
    let parts: Vec<&str> = s.trim().split('!').collect();
    match parts.as_slice() {
        [port] => (String::from("tcp"), String::from("*"), (*port).to_string()),
        [host, port] => (
            String::from("tcp"),
            (*host).to_string(),
            (*port).to_string(),
        ),
        [net, host, port, ..] => ((*net).to_string(), (*host).to_string(), (*port).to_string()),
        _ => (String::from("tcp"), String::new(), String::new()),
    }
}

fn host_port(addr: &str) -> Result<(String, u16)> {
    let (_net, host, port) = parse_addr(addr);
    let port: u16 = port.parse().map_err(|_| Error::new("bad port"))?;
    let host = host.trim_matches(|c| c == '[' || c == ']').to_string();
    Ok((host, port))
}

fn looks_numeric(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b':')
}
