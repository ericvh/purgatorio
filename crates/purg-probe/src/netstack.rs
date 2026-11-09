//! `#I` for the probe: a loopback for `127.0.0.1`, and `smoltcp` over
//! virtio-net for everything else. QEMU user networking gives the guest
//! `10.0.2.15` and routes through `10.0.2.2`.

use alloc::borrow::Cow;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use purg_platform::Result;
use purg_platform::net::{Conn, Listener, Net};
use purg_platform::sync::{Mutex, Rendez};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{self, State};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

use crate::hw;
use crate::virtio::VirtioNet;

const GUEST: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);

static NEXT_PORT: AtomicU16 = AtomicU16::new(49152);

fn ephemeral() -> u16 {
    let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    if port < 49152 { 49152 } else { port }
}

fn now() -> Instant {
    let hz = hw::counter_hz().max(1);
    let ms = (u128::from(hw::counter()) * 1000 / u128::from(hz)) as i64;
    Instant::from_millis(ms)
}

fn pause(ms: i32) -> Result<()> {
    let r = Rendez::new();
    purg_core::block::park_timeout(&r, || false, ms)
        .map(|_| ())
        .map_err(|e| Cow::Owned(format!("{e}")))
}

/// The probe's network. Loopback always works. The virtio stack is absent
/// when QEMU was started without a NIC.
pub struct ProbeNet {
    loopback: Loopback,
    smol: Option<Arc<Smol>>,
    down: &'static str,
}

impl ProbeNet {
    pub fn new() -> ProbeNet {
        match Stack::open() {
            Ok(stack) => ProbeNet {
                loopback: Loopback::new(),
                smol: Some(Arc::new(Smol {
                    inner: Mutex::new(stack),
                })),
                down: "",
            },
            Err(e) => ProbeNet {
                loopback: Loopback::new(),
                smol: None,
                down: e,
            },
        }
    }
}

impl Net for ProbeNet {
    fn dial_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        if is_loopback(host) {
            return self.loopback.dial(port);
        }
        let ip = parse_v4(host).ok_or(Cow::Borrowed("no translation"))?;
        self.stack()?
            .dial(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]), port)
    }

    fn announce_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>> {
        if is_loopback(host) {
            return self.loopback.announce(port);
        }
        if !(host.is_empty() || host == "*" || host == "0.0.0.0" || parse_v4(host).is_some()) {
            return Err(Cow::Borrowed("no translation"));
        }
        self.stack()?.listen(port)
    }

    fn dial_udp(&self, _host: &str, _port: u16) -> Result<Arc<dyn Conn>> {
        Err(Cow::Borrowed("not implemented"))
    }

    fn announce_udp(&self, _host: &str, _port: u16) -> Result<Arc<dyn Conn>> {
        Err(Cow::Borrowed("not implemented"))
    }

    fn resolve(&self, host: &str, _port: u16) -> Result<String> {
        if host == "*" || host.is_empty() || parse_v4(host).is_some() {
            Ok(String::from(host))
        } else {
            Err(Cow::Borrowed("no translation"))
        }
    }
}

impl ProbeNet {
    fn stack(&self) -> Result<Arc<Smol>> {
        self.smol
            .clone()
            .ok_or(Cow::Borrowed(if self.down.is_empty() {
                "network down"
            } else {
                self.down
            }))
    }
}

fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1"
}

fn parse_v4(host: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = host.split('.');
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        None
    } else {
        Some(out)
    }
}

struct Stack {
    dev: VirtioNet,
    iface: Interface,
    sockets: SocketSet<'static>,
    free: Vec<SocketHandle>,
}

impl Stack {
    fn open() -> core::result::Result<Stack, &'static str> {
        let mut dev = VirtioNet::open()?;
        let mac = dev.mac();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = hw::counter();
        let mut iface = Interface::new(config, &mut dev, now());
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::Ipv4(GUEST), 24)).ok();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY)
            .map_err(|_| "no route")?;
        let mut sockets = SocketSet::new(Vec::new());
        let mut free = Vec::new();
        for _ in 0..6 {
            free.push(sockets.add(new_socket()));
        }
        Ok(Stack {
            dev,
            iface,
            sockets,
            free,
        })
    }

    fn poll(&mut self) {
        let Stack {
            dev,
            iface,
            sockets,
            ..
        } = self;
        iface.poll(now(), dev, sockets);
    }

    fn alloc(&mut self) -> core::result::Result<SocketHandle, &'static str> {
        if let Some(handle) = self.free.pop() {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            if socket.is_open() {
                socket.abort();
            }
            return Ok(handle);
        }
        Ok(self.sockets.add(new_socket()))
    }
}

fn new_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 8192]),
        tcp::SocketBuffer::new(vec![0; 8192]),
    )
}

/// Virtio stack shared by listeners and connections.
pub struct Smol {
    inner: Mutex<Stack>,
}

impl Smol {
    fn lock(&self) -> purg_platform::sync::MutexGuard<'_, Stack> {
        self.inner.lock()
    }

    fn dial(self: &Arc<Self>, ip: Ipv4Address, port: u16) -> Result<Arc<dyn Conn>> {
        let handle = {
            let mut stack = self.lock();
            let handle = stack.alloc().map_err(Cow::Borrowed)?;
            let local = ephemeral();
            let Stack { iface, sockets, .. } = &mut *stack;
            sockets
                .get_mut::<tcp::Socket>(handle)
                .connect(iface.context(), (IpAddress::Ipv4(ip), port), local)
                .map_err(|_| Cow::Borrowed("connection refused"))?;
            handle
        };
        let deadline = now_ms() + 5_000;
        loop {
            {
                let mut stack = self.lock();
                stack.poll();
                let socket = stack.sockets.get::<tcp::Socket>(handle);
                match socket.state() {
                    State::Established => {
                        let local = endpoint(socket.local_endpoint());
                        let remote = endpoint(socket.remote_endpoint());
                        drop(stack);
                        return Ok(Arc::new(TcpConn {
                            smol: Arc::clone(self),
                            handle,
                            local,
                            remote,
                        }));
                    }
                    State::Closed => return Err(Cow::Borrowed("connection refused")),
                    _ => {}
                }
            }
            if now_ms() > deadline {
                return Err(Cow::Borrowed("connection timed out"));
            }
            pause(1)?;
        }
    }

    fn listen(self: &Arc<Self>, port: u16) -> Result<Arc<dyn Listener>> {
        let port = if port == 0 { ephemeral() } else { port };
        let handle = {
            let mut stack = self.lock();
            let handle = stack.alloc().map_err(Cow::Borrowed)?;
            stack
                .sockets
                .get_mut::<tcp::Socket>(handle)
                .listen(port)
                .map_err(|_| Cow::Borrowed("address in use"))?;
            handle
        };
        Ok(Arc::new(SmolListen {
            smol: Arc::clone(self),
            handle: Mutex::new(handle),
            port,
            local: format!("{GUEST}!{port}"),
        }))
    }
}

fn now_ms() -> i64 {
    let hz = hw::counter_hz().max(1);
    (u128::from(hw::counter()) * 1000 / u128::from(hz)) as i64
}

fn endpoint(ep: Option<IpEndpoint>) -> String {
    match ep {
        Some(ep) => format!("{}!{}", ep.addr, ep.port),
        None => String::new(),
    }
}

struct TcpConn {
    smol: Arc<Smol>,
    handle: SocketHandle,
    local: String,
    remote: String,
}

impl Conn for TcpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            {
                let mut stack = self.smol.lock();
                stack.poll();
                let socket = stack.sockets.get_mut::<tcp::Socket>(self.handle);
                match socket.recv_slice(buf) {
                    Ok(0) => {}
                    Ok(n) => return Ok(n),
                    Err(tcp::RecvError::Finished) => return Ok(0),
                    Err(tcp::RecvError::InvalidState) => {
                        if !matches!(
                            socket.state(),
                            State::Established | State::FinWait1 | State::FinWait2
                        ) {
                            return Ok(0);
                        }
                    }
                }
            }
            pause(1)?;
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            {
                let mut stack = self.smol.lock();
                stack.poll();
                let socket = stack.sockets.get_mut::<tcp::Socket>(self.handle);
                match socket.send_slice(buf) {
                    Ok(0) => {}
                    Ok(n) => return Ok(n),
                    Err(_) => {
                        if !socket.may_send() {
                            return Err(Cow::Borrowed("connection closed"));
                        }
                    }
                }
            }
            pause(1)?;
        }
    }

    fn local(&self) -> String {
        self.local.clone()
    }

    fn remote(&self) -> String {
        self.remote.clone()
    }

    fn hangup(&self) {
        let mut stack = self.smol.lock();
        let socket = stack.sockets.get_mut::<tcp::Socket>(self.handle);
        socket.close();
        stack.poll();
    }
}

struct SmolListen {
    smol: Arc<Smol>,
    handle: Mutex<SocketHandle>,
    port: u16,
    local: String,
}

impl Listener for SmolListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            let ready = {
                let mut stack = self.smol.lock();
                stack.poll();
                let handle = *self.handle.lock();
                let socket = stack.sockets.get::<tcp::Socket>(handle);
                if socket.state() == State::Established {
                    let local = endpoint(socket.local_endpoint());
                    let remote = endpoint(socket.remote_endpoint());
                    let born = stack.alloc().map_err(Cow::Borrowed)?;
                    stack
                        .sockets
                        .get_mut::<tcp::Socket>(born)
                        .listen(self.port)
                        .map_err(|_| Cow::Borrowed("address in use"))?;
                    *self.handle.lock() = born;
                    Some((handle, local, remote))
                } else {
                    None
                }
            };
            if let Some((handle, local, remote)) = ready {
                return Ok(Arc::new(TcpConn {
                    smol: Arc::clone(&self.smol),
                    handle,
                    local,
                    remote,
                }));
            }
            pause(1)?;
        }
    }

    fn local(&self) -> String {
        self.local.clone()
    }
}

/// In-process connections for `127.0.0.1`. The guest tests dial themselves.
struct Loopback {
    listeners: Mutex<Vec<(u16, Arc<LoopListen>)>>,
    next: AtomicU16,
}

impl Loopback {
    fn new() -> Loopback {
        Loopback {
            listeners: Mutex::new(Vec::new()),
            next: AtomicU16::new(1),
        }
    }

    fn announce(&self, port: u16) -> Result<Arc<dyn Listener>> {
        let port = if port == 0 {
            self.next.fetch_add(1, Ordering::Relaxed).max(1)
        } else {
            port
        };
        let listen = Arc::new(LoopListen {
            port,
            waiting: Mutex::new(VecDeque::new()),
        });
        self.listeners.lock().push((port, Arc::clone(&listen)));
        Ok(listen)
    }

    fn dial(&self, port: u16) -> Result<Arc<dyn Conn>> {
        let listen = self
            .listeners
            .lock()
            .iter()
            .find(|(p, _)| *p == port)
            .map(|(_, l)| Arc::clone(l))
            .ok_or(Cow::Borrowed("connection refused"))?;
        let client_in = Arc::new(Pipe::new());
        let server_in = Arc::new(Pipe::new());
        let local_port = self.next.fetch_add(1, Ordering::Relaxed).max(1);
        let server = Arc::new(LoopConn {
            mine: Arc::clone(&server_in),
            peer: Arc::clone(&client_in),
            local: format!("127.0.0.1!{port}"),
            remote: format!("127.0.0.1!{local_port}"),
        });
        let client = Arc::new(LoopConn {
            mine: client_in,
            peer: server_in,
            local: format!("127.0.0.1!{local_port}"),
            remote: format!("127.0.0.1!{port}"),
        });
        listen.waiting.lock().push_back(server);
        Ok(client)
    }
}

struct LoopListen {
    port: u16,
    waiting: Mutex<VecDeque<Arc<LoopConn>>>,
}

impl Listener for LoopListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            if let Some(conn) = self.waiting.lock().pop_front() {
                return Ok(conn);
            }
            pause(1)?;
        }
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.port)
    }
}

struct Pipe {
    q: Mutex<VecDeque<u8>>,
    closed: AtomicBool,
}

impl Pipe {
    fn new() -> Pipe {
        Pipe {
            q: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
        }
    }
}

struct LoopConn {
    mine: Arc<Pipe>,
    peer: Arc<Pipe>,
    local: String,
    remote: String,
}

impl Conn for LoopConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            {
                let mut q = self.mine.q.lock();
                if !q.is_empty() {
                    let n = q.len().min(buf.len());
                    for byte in &mut buf[..n] {
                        *byte = q.pop_front().unwrap_or(0);
                    }
                    return Ok(n);
                }
                if self.peer.closed.load(Ordering::Acquire) {
                    return Ok(0);
                }
            }
            pause(1)?;
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if self.peer.closed.load(Ordering::Acquire) {
            return Err(Cow::Borrowed("connection closed"));
        }
        self.peer.q.lock().extend(buf.iter().copied());
        Ok(buf.len())
    }

    fn local(&self) -> String {
        self.local.clone()
    }

    fn remote(&self) -> String {
        self.remote.clone()
    }

    fn hangup(&self) {
        self.mine.closed.store(true, Ordering::Release);
    }
}
