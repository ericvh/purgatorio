//! `#I`, the IP device. It serves `/net/tcp`, `/net/udp` and `/net/cs` the way
//! Inferno's `devip` and connection server do, and asks the platform's
//! [`purg_platform::net::Net`] to open the sockets.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_abi::{error, mode};
use purg_platform::net::{Conn, Listener, Net};
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

const PROTO_TCP: u8 = 1;
const PROTO_UDP: u8 = 2;

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
    path >> 40 != 0
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
            QROOT | QTCP | QUDP => QTDIR,
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
            QCS => (String::from("cs"), QTFILE, 0o666),
            q if q == qclone(PROTO_TCP) || q == qclone(PROTO_UDP) => {
                (String::from("clone"), QTFILE, 0o666)
            }
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
                "cs" => QCS,
                _ => return Err(Error::new(error::ENONEXIST)),
            },
            QTCP => proto_child(self, PROTO_TCP, name)?,
            QUDP => proto_child(self, PROTO_UDP, name)?,
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
        if c.qid.path == qclone(PROTO_TCP) || c.qid.path == qclone(PROTO_UDP) {
            let proto = if c.qid.path == qclone(PROTO_UDP) {
                PROTO_UDP
            } else {
                PROTO_TCP
            };
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
                entries.push(self.dirent(QTCP));
                entries.push(self.dirent(QUDP));
                entries.push(self.dirent(QCS));
            }
            QTCP => self.push_proto(&mut entries, PROTO_TCP),
            QUDP => self.push_proto(&mut entries, PROTO_UDP),
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
        let (host, port) = host_port(addr)?;
        let net = self.net()?;
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
        let (host, port) = host_port(addr)?;
        let host = if host.is_empty() {
            String::from("*")
        } else {
            host
        };
        let net = self.net()?;
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
