//! Host sockets and processes for `#I` and `#C`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use purg_abi::error::EINTR;
use purg_platform::Result;
use purg_platform::net::{self, Conn, Listener};
use purg_platform::sync::current_note;

pub struct HostNet;

impl net::Net for HostNet {
    fn dial_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        let stream = TcpStream::connect((host, port)).map_err(io_err)?;
        stream.set_nodelay(true).ok();
        stream.set_nonblocking(true).map_err(io_err)?;
        let local = endpoint(stream.local_addr().map_err(io_err)?);
        let remote = endpoint(stream.peer_addr().map_err(io_err)?);
        Ok(Arc::new(TcpConn {
            stream: Mutex::new(stream),
            local,
            remote,
            dead: AtomicBool::new(false),
        }))
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

    fn announce_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        let sock = if host.is_empty() || host == "*" {
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(io_err)?
        } else {
            UdpSocket::bind((host, port)).map_err(io_err)?
        };
        sock.set_nonblocking(true).map_err(io_err)?;
        let local = endpoint(sock.local_addr().map_err(io_err)?);
        Ok(Arc::new(UdpConn {
            sock: Mutex::new(sock),
            local,
            remote: String::new(),
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

struct TcpConn {
    stream: Mutex<TcpStream>,
    local: String,
    remote: String,
    dead: AtomicBool,
}

impl Conn for TcpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if self.dead.load(Ordering::Acquire) {
                return Ok(0);
            }
            interrupted()?;
            let mut stream = self.stream.lock().map_err(|_| poison())?;
            match stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    drop(stream);
                    std::thread::sleep(Duration::from_millis(5));
                }
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
            let mut stream = self.stream.lock().map_err(|_| poison())?;
            match stream.write(buf) {
                Ok(0) => return Err(std::borrow::Cow::Borrowed("connection closed")),
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    drop(stream);
                    std::thread::sleep(Duration::from_millis(5));
                }
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
        if let Ok(stream) = self.stream.lock() {
            stream.shutdown(Shutdown::Both).ok();
        }
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
    stream.set_nonblocking(true).map_err(io_err)?;
    let local = endpoint(stream.local_addr().map_err(io_err)?);
    let remote = endpoint(stream.peer_addr().map_err(io_err)?);
    Ok(Arc::new(TcpConn {
        stream: Mutex::new(stream),
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
