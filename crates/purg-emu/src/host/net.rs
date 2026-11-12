//! Host sockets and processes for `#I` and `#C`.

use std::io::{Read, Write};
use std::net::{
    Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use purg_abi::error::EINTR;
use purg_platform::Result;
use purg_platform::net::{self, Conn, Listener, UdpDemux};
use purg_platform::sync::current_note;

use super::os;

pub struct HostNet;

impl net::Net for HostNet {
    fn dial_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        let stream = TcpStream::connect((host, port)).map_err(io_err)?;
        tcp_conn(stream)
    }

    fn announce_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>> {
        if host.is_empty() || host == "*" {
            return announce_any(port);
        }
        let listener = TcpListener::bind((host, port)).map_err(io_err)?;
        listener.set_nonblocking(true).map_err(io_err)?;
        let local = endpoint(listener.local_addr().map_err(io_err)?);
        Ok(Arc::new(OneListener { listener, local }))
    }

    fn dial_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(io_err)?;
        sock.connect((host, port)).map_err(io_err)?;
        sock.set_nonblocking(true).map_err(io_err)?;
        let local = endpoint(sock.local_addr().map_err(io_err)?);
        let remote = endpoint(sock.peer_addr().map_err(io_err)?);
        Ok(Arc::new(UdpConn {
            sock: Mutex::new(sock),
            local,
            remote,
        }))
    }

    fn announce_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>> {
        let sock = if host.is_empty() || host == "*" {
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(io_err)?
        } else {
            UdpSocket::bind((host, port)).map_err(io_err)?
        };
        sock.set_nonblocking(true).map_err(io_err)?;
        let local = endpoint(sock.local_addr().map_err(io_err)?);
        Ok(Arc::new(UdpListen {
            port: Arc::new(UdpPort {
                sock,
                demux: Mutex::new(UdpDemux::new(true)),
            }),
            local,
        }))
    }

    fn resolve(&self, host: &str, port: u16) -> Result<String> {
        if host == "*" || host.is_empty() || is_numeric(host) {
            return Ok(String::from(host));
        }
        let mut addrs = (host, port)
            .to_socket_addrs()
            .map_err(|_| no_translation())?;
        let addr = addrs.next().ok_or_else(no_translation)?;
        Ok(addr.ip().to_string())
    }

    fn dial_icmp(&self, host: &str) -> Result<Arc<dyn Conn>> {
        dial_icmp(host)
    }

    fn announce_udp_headers(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        let sock = if host.is_empty() || host == "*" {
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(io_err)?
        } else {
            UdpSocket::bind((host, port)).map_err(io_err)?
        };
        sock.set_broadcast(true).map_err(io_err)?;
        sock.set_read_timeout(Some(TCP_POLL)).map_err(io_err)?;
        let local = match sock.local_addr().map_err(io_err)? {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => return Err(std::borrow::Cow::Borrowed("headers need IPv4")),
        };
        Ok(Arc::new(UdpHeaders { sock, local }))
    }
}

fn announce_any(port: u16) -> Result<Arc<dyn Listener>> {
    let v4 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(io_err)?;
    v4.set_nonblocking(true).map_err(io_err)?;
    let chosen = v4.local_addr().map_err(io_err)?.port();
    let v6 = TcpListener::bind((Ipv6Addr::UNSPECIFIED, chosen)).ok();
    if let Some(v6) = &v6 {
        v6.set_nonblocking(true).ok();
    }
    Ok(Arc::new(AnyListener {
        v4,
        v6,
        local: format!("*!{chosen}"),
    }))
}

/// How long a blocked TCP read or write waits before it looks again for
/// a note (a kill) or a hangup. Data wakes it at once.
const TCP_POLL: Duration = Duration::from_millis(5);

/// A TCP connection. The socket blocks, with [`TCP_POLL`] as its read and
/// write timeouts, so a read returns as soon as data arrives and a killed
/// prog still gets out within one timeout. (A non-blocking socket polled
/// with a sleep added the sleep to every round trip.) Reads and writes go
/// through `&TcpStream`, so a reader blocked on one thread does not hold
/// up a writer on another.
struct TcpConn {
    stream: TcpStream,
    local: String,
    remote: String,
    dead: AtomicBool,
}

/// A timeout of the blocking socket: nothing yet, look again.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

impl Conn for TcpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if self.dead.load(Ordering::Acquire) {
                return Ok(0);
            }
            interrupted()?;
            match (&self.stream).read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if timed_out(&e) => {}
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            interrupted()?;
            match (&self.stream).write(buf) {
                Ok(0) => return Err(std::borrow::Cow::Borrowed("connection closed")),
                Ok(n) => return Ok(n),
                Err(e) if timed_out(&e) => {}
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    fn local(&self) -> String {
        self.local.clone()
    }

    fn remote(&self) -> String {
        self.remote.clone()
    }

    fn hangup(&self) {
        self.dead.store(true, Ordering::Release);
        self.stream.shutdown(Shutdown::Both).ok();
    }
}

struct OneListener {
    listener: TcpListener,
    local: String,
}

impl Listener for OneListener {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        accept_one(&self.listener)
    }

    fn local(&self) -> String {
        self.local.clone()
    }
}

struct AnyListener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    local: String,
}

impl Listener for AnyListener {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            interrupted()?;
            match self.v4.accept() {
                Ok((stream, _)) => return tcp_conn(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(io_err(e)),
            }
            if let Some(v6) = &self.v6 {
                match v6.accept() {
                    Ok((stream, _)) => return tcp_conn(stream),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(io_err(e)),
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn local(&self) -> String {
        self.local.clone()
    }
}

fn accept_one(listener: &TcpListener) -> Result<Arc<dyn Conn>> {
    loop {
        interrupted()?;
        match listener.accept() {
            Ok((stream, _)) => return tcp_conn(stream),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(io_err(e)),
        }
    }
}

fn tcp_conn(stream: TcpStream) -> Result<Arc<dyn Conn>> {
    stream.set_nodelay(true).ok();
    // An accepted socket inherits the listener's non-blocking flag on BSD
    // and macOS, not on Linux; set it either way.
    stream.set_nonblocking(false).map_err(io_err)?;
    stream.set_read_timeout(Some(TCP_POLL)).map_err(io_err)?;
    stream.set_write_timeout(Some(TCP_POLL)).map_err(io_err)?;
    let local = endpoint(stream.local_addr().map_err(io_err)?);
    let remote = endpoint(stream.peer_addr().map_err(io_err)?);
    Ok(Arc::new(TcpConn {
        stream,
        local,
        remote,
        dead: AtomicBool::new(false),
    }))
}

struct UdpConn {
    sock: Mutex<UdpSocket>,
    local: String,
    remote: String,
}

impl Conn for UdpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            interrupted()?;
            let sock = self.sock.lock().map_err(|_| poison())?;
            match sock.recv(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    drop(sock);
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let sock = self.sock.lock().map_err(|_| poison())?;
        sock.send(buf).map_err(io_err)
    }

    fn local(&self) -> String {
        self.local.clone()
    }

    fn remote(&self) -> String {
        self.remote.clone()
    }

    fn hangup(&self) {}
}

/// An announced UDP socket, whose datagrams [`UdpDemux`] sorts into one
/// conversation per remote.
struct UdpPort {
    sock: UdpSocket,
    demux: Mutex<UdpDemux<SocketAddr>>,
}

impl UdpPort {
    /// Moves the datagrams waiting on the socket to their conversations.
    fn pump(&self, demux: &mut UdpDemux<SocketAddr>) -> Result<()> {
        let mut buf = vec![0u8; 65536];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    demux.deliver(from, buf[..n].to_vec());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                // A port-unreachable from an earlier send; nothing to file.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    /// Pumps and runs `f` on the demux until it returns `Some`.
    fn wait<T>(&self, mut f: impl FnMut(&mut UdpDemux<SocketAddr>) -> Option<T>) -> Result<T> {
        loop {
            interrupted()?;
            {
                let mut demux = self.demux.lock().map_err(|_| poison())?;
                self.pump(&mut demux)?;
                if let Some(v) = f(&mut demux) {
                    return Ok(v);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct UdpListen {
    port: Arc<UdpPort>,
    local: String,
}

impl Listener for UdpListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        let (id, remote) = self.port.wait(|d| d.accept())?;
        Ok(Arc::new(UdpChild {
            port: Arc::clone(&self.port),
            id,
            remote,
            local: self.local.clone(),
            closed: AtomicBool::new(false),
        }))
    }

    fn local(&self) -> String {
        self.local.clone()
    }
}

/// One remote's conversation on an announced UDP port.
struct UdpChild {
    port: Arc<UdpPort>,
    id: u64,
    remote: SocketAddr,
    local: String,
    closed: AtomicBool,
}

impl Conn for UdpChild {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.port.wait(|d| {
            if self.closed.load(Ordering::Acquire) {
                Some(0)
            } else {
                d.recv(self.id, buf)
            }
        })
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(std::borrow::Cow::Borrowed("connection closed"));
        }
        self.port.sock.send_to(buf, self.remote).map_err(io_err)
    }

    fn local(&self) -> String {
        self.local.clone()
    }

    fn remote(&self) -> String {
        endpoint(self.remote)
    }

    fn hangup(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(mut d) = self.port.demux.lock() {
            d.forget(self.id);
        }
    }
}

/// An announced UDP port in `headers` mode: one conversation has every
/// datagram, each after a header naming its sender, and each write names
/// its destination the same way. The socket blocks with [`TCP_POLL`] as
/// its timeout, so a killed prog gets out.
struct UdpHeaders {
    sock: UdpSocket,
    local: std::net::SocketAddrV4,
}

impl Conn for UdpHeaders {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut pkt = vec![0u8; 65536];
        loop {
            interrupted()?;
            let (n, from) = match self.sock.recv_from(&mut pkt) {
                Ok((n, SocketAddr::V4(from))) => (n, from),
                Ok(_) => continue,
                Err(e) if timed_out(&e) => continue,
                // A port-unreachable from an earlier send; nothing to read.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                Err(e) => return Err(io_err(e)),
            };
            let mut whole = net::udp_header(
                from.ip().octets(),
                from.port(),
                self.local.ip().octets(),
                self.local.port(),
            )
            .to_vec();
            whole.extend_from_slice(&pkt[..n]);
            let k = whole.len().min(buf.len());
            buf[..k].copy_from_slice(&whole[..k]);
            return Ok(k);
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let (addr, port) =
            net::udp_header_remote(buf).ok_or(std::borrow::Cow::Borrowed("bad udp header"))?;
        self.sock
            .send_to(&buf[net::UDP_HEADER..], (Ipv4Addr::from(addr), port))
            .map_err(io_err)?;
        Ok(buf.len())
    }

    fn local(&self) -> String {
        endpoint(SocketAddr::V4(self.local))
    }

    fn remote(&self) -> String {
        String::new()
    }

    fn hangup(&self) {}
}

/// Identifiers for the emu's ICMP conversations, where the host does not
/// assign its own.
static ICMP_IDENT: AtomicU16 = AtomicU16::new(0);

/// An ICMP echo conversation on an unprivileged ICMP datagram socket
/// (`os::icmp_socket`). Reads block with [`TCP_POLL`] as the socket's
/// timeout, so a killed prog still gets out.
struct IcmpConn {
    sock: UdpSocket,
    remote: Ipv4Addr,
    local: Ipv4Addr,
    ident: u16,
}

fn dial_icmp(host: &str) -> Result<Arc<dyn Conn>> {
    let remote = match host.parse::<Ipv4Addr>() {
        Ok(a) => a,
        Err(_) => (host, 0)
            .to_socket_addrs()
            .map_err(|_| no_translation())?
            .find_map(|a| match a {
                SocketAddr::V4(v4) => Some(*v4.ip()),
                SocketAddr::V6(_) => None,
            })
            .ok_or_else(no_translation)?,
    };
    let fd = os::icmp_socket().map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied
            || e.raw_os_error() == Some(libc::EPROTONOSUPPORT)
        {
            std::borrow::Cow::Borrowed(os::ICMP_DENIED)
        } else {
            io_err(e)
        }
    })?;
    let sock = UdpSocket::from(fd);
    sock.set_read_timeout(Some(TCP_POLL)).map_err(io_err)?;
    // The address the host sends from, found with a UDP socket that is
    // connected but never used.
    let local = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|u| {
            u.connect((remote, 9))?;
            u.local_addr()
        })
        .map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
        })
        .unwrap_or(Ipv4Addr::UNSPECIFIED);
    let ident = (std::process::id() as u16)
        .wrapping_add(ICMP_IDENT.fetch_add(1, Ordering::AcqRel).wrapping_mul(7919));
    Ok(Arc::new(IcmpConn {
        sock,
        remote,
        local,
        ident,
    }))
}

impl Conn for IcmpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut pkt = vec![0u8; 65536];
        loop {
            interrupted()?;
            let got = match os::icmp_recv(&self.sock, &mut pkt) {
                Ok(got) => got,
                Err(e) if timed_out(&e) || e.kind() == std::io::ErrorKind::InvalidData => {
                    continue;
                }
                Err(e) => return Err(io_err(e)),
            };
            let msg = &pkt[got.start..got.start + got.len];
            if got.src != self.remote || msg.len() < 8 || msg[0] != net::ICMP_ECHO_REPLY {
                continue;
            }
            if !os::ICMP_IDENT_BY_HOST && u16::from_be_bytes([msg[4], msg[5]]) != self.ident {
                continue;
            }
            let mut whole =
                net::ipv4_header(got.src.octets(), self.local.octets(), got.ttl, msg.len())
                    .to_vec();
            whole.extend_from_slice(msg);
            let n = whole.len().min(buf.len());
            buf[..n].copy_from_slice(&whole[..n]);
            return Ok(n);
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let mut msg = buf.to_vec();
        if msg.len() >= 8 {
            msg[4..6].copy_from_slice(&self.ident.to_be_bytes());
        }
        net::icmp_fill_checksum(&mut msg);
        self.sock
            .send_to(&msg, (self.remote, 0))
            .map_err(io_err)
            .map(|_| buf.len())
    }

    fn local(&self) -> String {
        format!("{}!{}", self.local, self.ident)
    }

    fn remote(&self) -> String {
        format!("{}!1", self.remote)
    }

    fn hangup(&self) {}
}

/// A host process behind `#C`.
struct Proc {
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<ChildStdout>,
    stderr: Mutex<ChildStderr>,
    child: Mutex<Option<Child>>,
    done: Mutex<Option<String>>,
}

impl net::Command for Proc {
    fn read_stdout(&self, buf: &mut [u8]) -> Result<usize> {
        let mut out = self.stdout.lock().map_err(|_| poison())?;
        out.read(buf).map_err(io_err)
    }

    fn read_stderr(&self, buf: &mut [u8]) -> Result<usize> {
        let mut err = self.stderr.lock().map_err(|_| poison())?;
        err.read(buf).map_err(io_err)
    }

    fn write_stdin(&self, buf: &[u8]) -> Result<usize> {
        let mut stdin = self.stdin.lock().map_err(|_| poison())?;
        let Some(stdin) = stdin.as_mut() else {
            return Err(std::borrow::Cow::Borrowed("connection closed"));
        };
        stdin.write(buf).map_err(io_err)
    }

    fn wait(&self) -> Result<String> {
        if let Some(status) = self.done.lock().map_err(|_| poison())?.clone() {
            return Ok(status);
        }
        let child = self.child.lock().map_err(|_| poison())?.take();
        let Some(mut child) = child else {
            return Ok(self
                .done
                .lock()
                .map_err(|_| poison())?
                .clone()
                .unwrap_or_default());
        };
        let status = child.wait().map_err(io_err)?;
        let text = match status.code() {
            Some(0) => String::new(),
            Some(n) => format!("exit {n}"),
            None => String::from("killed"),
        };
        *self.done.lock().map_err(|_| poison())? = Some(text.clone());
        Ok(text)
    }

    fn status(&self) -> String {
        if let Some(status) = self.done.lock().ok().and_then(|g| g.clone()) {
            return if status.is_empty() {
                String::from("Done")
            } else {
                status
            };
        }
        String::from("Running")
    }
}

pub fn start_command(line: &str) -> Result<Arc<dyn net::Command>> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(line)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_err)?;
    let stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or(std::borrow::Cow::Borrowed("no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(std::borrow::Cow::Borrowed("no stderr"))?;
    Ok(Arc::new(Proc {
        stdin: Mutex::new(stdin),
        stdout: Mutex::new(stdout),
        stderr: Mutex::new(stderr),
        child: Mutex::new(Some(child)),
        done: Mutex::new(None),
    }))
}

fn endpoint(addr: std::net::SocketAddr) -> String {
    format!("{}!{}", addr.ip(), addr.port())
}

fn is_numeric(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b':')
}

fn interrupted() -> Result<()> {
    if current_note().is_some_and(|n| n.killed()) {
        Err(std::borrow::Cow::Borrowed(EINTR))
    } else {
        Ok(())
    }
}

fn io_err(e: std::io::Error) -> std::borrow::Cow<'static, str> {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => std::borrow::Cow::Borrowed("connection refused"),
        std::io::ErrorKind::TimedOut => std::borrow::Cow::Borrowed("connection timed out"),
        std::io::ErrorKind::AddrInUse => std::borrow::Cow::Borrowed("address in use"),
        std::io::ErrorKind::Interrupted => std::borrow::Cow::Borrowed(EINTR),
        _ => std::borrow::Cow::Owned(e.to_string()),
    }
}

fn no_translation() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("no translation")
}

fn poison() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("lock poisoned")
}
