//! A [`Net`] for `#I` on bare metal: in-process loopback for `127.0.0.1`,
//! and `smoltcp` over a [`VirtioNet`] for everything else.
//!
//! ```ignore
//! let net = NetStack::open(&fdt, &Ipv4Config::QEMU_USER);
//! let platform = MetalPlatform::new(...).with_net(Box::new(net));
//! ```
//!
//! [`NetStack::open`] finds the card in the DTB and routes its interrupt
//! (so call it after [`irq::init`]). Without a card, or if it does not come
//! up, loopback still works and other addresses fail with the reason.
//!
//! Waiting: every operation polls the interface, and if it cannot finish
//! sleeps until the card interrupts (a frame arrived or was sent) or
//! [`MAX_WAIT_MS`] passes, so TCP timers still run. A killed prog's sleep
//! ends with `interrupted`, as with `purg_core::block::park`.
//!
//! The interface has one IPv4 address and a default route ([`Ipv4Config`]);
//! there is no DHCP, and ARP for the gateway is smoltcp's.
//!
//! UDP follows Inferno: a dialed conversation reads and writes datagrams
//! with one remote, and an announced port starts a conversation for each
//! new remote, which `listen` returns ([`UdpDemux`]). There is no IP
//! fragmentation, so a datagram is at most [`UDP_MAX`] bytes.

use alloc::borrow::Cow;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use purg_platform::Result;
use purg_platform::net::{Conn, Listener, Net, UdpDemux};
use purg_platform::sync::{Mutex, MutexGuard, Rendez};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{self, State};
use smoltcp::socket::udp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};
use spin::Once;

use crate::dtb::Fdt;
use crate::irq::{self, After};
use crate::timer;
use crate::virtio::{self, Mmio, VirtioNet, sleep};

/// The longest a waiting operation sleeps before polling again.
pub const MAX_WAIT_MS: u64 = 10;

/// TCP sockets made up front; more are added as needed.
const SOCKETS: usize = 6;
const SOCKET_BUF: usize = 8192;
const FIRST_EPHEMERAL: u16 = 49152;
/// Datagrams each UDP socket buffers each way.
const UDP_PACKETS: usize = 16;
/// The largest UDP payload: the 1500-byte IP MTU less the IP and UDP
/// headers. Larger writes fail.
pub const UDP_MAX: usize = 1472;

/// The interface's static IPv4 configuration.
#[derive(Clone, Copy, Debug)]
pub struct Ipv4Config {
    pub addr: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
}

impl Ipv4Config {
    /// QEMU user networking (`-netdev user`): the guest is 10.0.2.15/24,
    /// routed through 10.0.2.2.
    pub const QEMU_USER: Ipv4Config = Ipv4Config {
        addr: [10, 0, 2, 15],
        prefix: 24,
        gateway: [10, 0, 2, 2],
    };
}

/// Bumped by the card's interrupt; sleepers wait for it to change.
static EVENTS: AtomicU64 = AtomicU64::new(0);
/// Woken by the card's interrupt.
static WAKE: Once<Rendez> = Once::new();
/// The card's registers, for the interrupt handler.
static IRQ_MMIO: Once<Mmio> = Once::new();

fn card_irq() -> After {
    if let Some(m) = IRQ_MMIO.get() {
        m.ack();
    }
    EVENTS.fetch_add(1, Ordering::Release);
    if let Some(r) = WAKE.get() {
        r.wakeup();
    }
    After::Return
}

fn now() -> Instant {
    Instant::from_millis(now_ms())
}

fn now_ms() -> i64 {
    (timer::ticks_to_ns(timer::counter()) / 1_000_000) as i64
}

/// The network: loopback always, smoltcp when there is a card.
pub struct NetStack {
    loopback: Loopback,
    smol: core::result::Result<Arc<Smol>, &'static str>,
}

impl NetStack {
    /// Brings up the first virtio-net card in the DTB with `cfg`, and
    /// routes its interrupt. Call once, after [`irq::init`].
    pub fn open(fdt: &Fdt<'_>, cfg: &Ipv4Config) -> NetStack {
        let smol = virtio::find(fdt, virtio::DEVICE_NET)
            .ok_or("no virtio-net device")
            .and_then(|t| {
                let dev = VirtioNet::new(&t)?;
                let stack = Stack::open(dev, cfg)?;
                IRQ_MMIO.call_once(|| t.mmio);
                WAKE.call_once(Rendez::new);
                irq::register(t.intid, card_irq);
                Ok(Arc::new(Smol {
                    inner: Mutex::new(stack),
                    local: Ipv4Address::from(cfg.addr),
                }))
            });
        NetStack {
            loopback: Loopback::new(),
            smol,
        }
    }

    /// Why there is no network beyond loopback, if there is not.
    pub fn down(&self) -> Option<&'static str> {
        self.smol.as_ref().err().copied()
    }

    fn stack(&self) -> Result<Arc<Smol>> {
        self.smol.clone().map_err(Cow::Borrowed)
    }
}

impl Net for NetStack {
    fn dial_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        if is_loopback(host) {
            return self.loopback.dial(port);
        }
        let ip = parse_v4(host).ok_or(Cow::Borrowed("no translation"))?;
        self.stack()?.dial(Ipv4Address::from(ip), port)
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

    fn dial_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        if is_loopback(host) {
            return self.loopback.dial_udp(port);
        }
        let ip = parse_v4(host).ok_or(Cow::Borrowed("no translation"))?;
        self.stack()?.dial_udp(Ipv4Address::from(ip), port)
    }

    fn announce_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>> {
        if is_loopback(host) {
            return self.loopback.announce_udp(port);
        }
        if !(host.is_empty() || host == "*" || host == "0.0.0.0" || parse_v4(host).is_some()) {
            return Err(Cow::Borrowed("no translation"));
        }
        self.stack()?.announce_udp(port)
    }

    fn resolve(&self, host: &str, _port: u16) -> Result<String> {
        if host == "*" || host.is_empty() || parse_v4(host).is_some() {
            Ok(String::from(host))
        } else {
            Err(Cow::Borrowed("no translation"))
        }
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
    next_port: u16,
}

impl Stack {
    fn open(mut dev: VirtioNet, cfg: &Ipv4Config) -> core::result::Result<Stack, &'static str> {
        let mac = dev.mac();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = timer::counter();
        let mut iface = Interface::new(config, &mut dev, now());
        let addr = Ipv4Address::from(cfg.addr);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(addr), cfg.prefix))
                .ok();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::from(cfg.gateway))
            .map_err(|_| "no route")?;
        let mut sockets = SocketSet::new(Vec::new());
        let mut free = Vec::new();
        for _ in 0..SOCKETS {
            free.push(sockets.add(new_socket()));
        }
        Ok(Stack {
            dev,
            iface,
            sockets,
            free,
            next_port: FIRST_EPHEMERAL,
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

    /// How long to sleep before smoltcp next needs a poll, capped.
    fn delay_ms(&mut self) -> u64 {
        self.iface
            .poll_delay(now(), &self.sockets)
            .map_or(MAX_WAIT_MS, |d| d.total_millis().clamp(1, MAX_WAIT_MS))
    }

    fn ephemeral(&mut self) -> u16 {
        let port = self.next_port;
        self.next_port = self.next_port.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
        port
    }

    fn alloc(&mut self) -> SocketHandle {
        if let Some(handle) = self.free.pop() {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            if socket.is_open() {
                socket.abort();
            }
            return handle;
        }
        self.sockets.add(new_socket())
    }
}

fn new_udp_socket() -> udp::Socket<'static> {
    let buf = || {
        udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_PACKETS],
            vec![0; UDP_PACKETS * UDP_MAX],
        )
    };
    udp::Socket::new(buf(), buf())
}

fn new_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; SOCKET_BUF]),
        tcp::SocketBuffer::new(vec![0; SOCKET_BUF]),
    )
}

/// The smoltcp stack, shared by listeners and connections.
struct Smol {
    inner: Mutex<Stack>,
    local: Ipv4Address,
}

impl Smol {
    fn lock(&self) -> MutexGuard<'_, Stack> {
        self.inner.lock()
    }

    /// Polls and runs `f` on the stack until it returns `Some`, sleeping
    /// between tries until the card interrupts or smoltcp's next timer.
    fn run<T>(&self, mut f: impl FnMut(&mut Stack) -> Option<Result<T>>) -> Result<T> {
        loop {
            let seen = EVENTS.load(Ordering::Acquire);
            let delay = {
                let mut stack = self.lock();
                stack.poll();
                if let Some(r) = f(&mut stack) {
                    return r;
                }
                stack.delay_ms()
            };
            let changed = || EVENTS.load(Ordering::Acquire) != seen;
            match WAKE.get() {
                Some(w) => {
                    sleep(w, changed, Some(delay))?;
                }
                None => return Err(Cow::Borrowed("network down")),
            }
        }
    }

    fn dial(self: &Arc<Self>, ip: Ipv4Address, port: u16) -> Result<Arc<dyn Conn>> {
        let handle = {
            let mut stack = self.lock();
            let handle = stack.alloc();
            let local = stack.ephemeral();
            let Stack { iface, sockets, .. } = &mut *stack;
            if sockets
                .get_mut::<tcp::Socket>(handle)
                .connect(iface.context(), (IpAddress::Ipv4(ip), port), local)
                .is_err()
            {
                stack.free.push(handle);
                return Err(Cow::Borrowed("connection refused"));
            }
            handle
        };
        let deadline = now_ms() + 5_000;
        let r = self.run(|stack| {
            let socket = stack.sockets.get::<tcp::Socket>(handle);
            match socket.state() {
                State::Established => Some(Ok((
                    endpoint(socket.local_endpoint()),
                    endpoint(socket.remote_endpoint()),
                ))),
                State::Closed => Some(Err(Cow::Borrowed("connection refused"))),
                _ if now_ms() > deadline => Some(Err(Cow::Borrowed("connection timed out"))),
                _ => None,
            }
        });
        match r {
            Ok((local, remote)) => Ok(Arc::new(TcpConn {
                smol: Arc::clone(self),
                handle,
                local,
                remote,
            })),
            Err(e) => {
                let mut stack = self.lock();
                stack.sockets.get_mut::<tcp::Socket>(handle).abort();
                stack.free.push(handle);
                Err(e)
            }
        }
    }

    fn listen(self: &Arc<Self>, port: u16) -> Result<Arc<dyn Listener>> {
        let mut stack = self.lock();
        let port = if port == 0 { stack.ephemeral() } else { port };
        let handle = stack.alloc();
        if stack
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .listen(port)
            .is_err()
        {
            stack.free.push(handle);
            return Err(Cow::Borrowed("address in use"));
        }
        drop(stack);
        Ok(Arc::new(SmolListen {
            smol: Arc::clone(self),
            handle: Mutex::new(handle),
            port,
            local: format!("{}!{port}", self.local),
        }))
    }
}

impl Smol {
    /// Opens a UDP socket on `port` (0: an ephemeral one).
    fn udp_port(self: &Arc<Self>, port: u16, announced: bool) -> Result<Arc<SmolUdp>> {
        let mut stack = self.lock();
        let port = if port == 0 { stack.ephemeral() } else { port };
        let mut socket = new_udp_socket();
        if socket.bind(port).is_err() {
            return Err(Cow::Borrowed("address in use"));
        }
        let handle = stack.sockets.add(socket);
        Ok(Arc::new(SmolUdp {
            smol: Arc::clone(self),
            handle,
            demux: Mutex::new(UdpDemux::new(announced)),
            local: format!("{}!{port}", self.local),
            open: AtomicBool::new(true),
        }))
    }

    fn dial_udp(self: &Arc<Self>, ip: Ipv4Address, port: u16) -> Result<Arc<dyn Conn>> {
        let sock = self.udp_port(0, false)?;
        let remote = IpEndpoint::new(IpAddress::Ipv4(ip), port);
        let id = sock.demux.lock().connect(remote);
        Ok(Arc::new(SmolUdpConn::new(sock, id, remote, true)))
    }

    fn announce_udp(self: &Arc<Self>, port: u16) -> Result<Arc<dyn Listener>> {
        Ok(Arc::new(SmolUdpListen {
            sock: self.udp_port(port, true)?,
        }))
    }
}

/// One smoltcp UDP socket and the conversations on it.
struct SmolUdp {
    smol: Arc<Smol>,
    handle: SocketHandle,
    demux: Mutex<UdpDemux<IpEndpoint>>,
    local: String,
    /// Whether `handle` is still in the socket set.
    open: AtomicBool,
}

impl SmolUdp {
    /// Moves the datagrams the socket has received to their conversations.
    fn pump(&self, stack: &mut Stack) {
        if !self.open.load(Ordering::Acquire) {
            return;
        }
        let socket = stack.sockets.get_mut::<udp::Socket>(self.handle);
        let mut demux = self.demux.lock();
        while let Ok((data, meta)) = socket.recv() {
            demux.deliver(meta.endpoint, data.to_vec());
        }
    }

    fn close(&self, stack: &mut Stack) {
        if self.open.swap(false, Ordering::AcqRel) {
            stack.sockets.remove(self.handle);
        }
    }
}

impl Drop for SmolUdp {
    fn drop(&mut self) {
        let smol = Arc::clone(&self.smol);
        self.close(&mut smol.lock());
    }
}

struct SmolUdpListen {
    sock: Arc<SmolUdp>,
}

impl Listener for SmolUdpListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        let (id, remote) = self.sock.smol.run(|stack| {
            self.sock.pump(stack);
            self.sock.demux.lock().accept().map(Ok)
        })?;
        Ok(Arc::new(SmolUdpConn::new(
            Arc::clone(&self.sock),
            id,
            remote,
            false,
        )))
    }

    fn local(&self) -> String {
        self.sock.local.clone()
    }
}

/// A UDP conversation: a dialed socket, or one remote of an announced one.
struct SmolUdpConn {
    sock: Arc<SmolUdp>,
    id: u64,
    remote: IpEndpoint,
    /// The socket is this conversation's alone, so hangup closes it.
    dialed: bool,
    closed: AtomicBool,
}

impl SmolUdpConn {
    fn new(sock: Arc<SmolUdp>, id: u64, remote: IpEndpoint, dialed: bool) -> SmolUdpConn {
        SmolUdpConn {
            sock,
            id,
            remote,
            dialed,
            closed: AtomicBool::new(false),
        }
    }
}

impl Conn for SmolUdpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.sock.smol.run(|stack| {
            if self.closed.load(Ordering::Acquire) {
                return Some(Ok(0));
            }
            self.sock.pump(stack);
            self.sock.demux.lock().recv(self.id, buf).map(Ok)
        })
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if buf.len() > UDP_MAX {
            return Err(Cow::Borrowed("message too long"));
        }
        self.sock.smol.run(|stack| {
            if self.closed.load(Ordering::Acquire) || !self.sock.open.load(Ordering::Acquire) {
                return Some(Err(Cow::Borrowed("connection closed")));
            }
            let socket = stack.sockets.get_mut::<udp::Socket>(self.sock.handle);
            match socket.send_slice(buf, self.remote) {
                Ok(()) => {
                    // Send now rather than at the next poll.
                    stack.poll();
                    Some(Ok(buf.len()))
                }
                Err(udp::SendError::BufferFull) => None,
                Err(udp::SendError::Unaddressable) => Some(Err(Cow::Borrowed("no route"))),
            }
        })
    }

    fn local(&self) -> String {
        self.sock.local.clone()
    }

    fn remote(&self) -> String {
        endpoint(Some(self.remote))
    }

    fn hangup(&self) {
        self.closed.store(true, Ordering::Release);
        let mut stack = self.sock.smol.lock();
        self.sock.demux.lock().forget(self.id);
        if self.dialed {
            self.sock.close(&mut stack);
        }
    }
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
        self.smol.run(|stack| {
            let socket = stack.sockets.get_mut::<tcp::Socket>(self.handle);
            match socket.recv_slice(buf) {
                Ok(0) => None,
                Ok(n) => Some(Ok(n)),
                Err(tcp::RecvError::Finished) => Some(Ok(0)),
                Err(tcp::RecvError::InvalidState) => (!matches!(
                    socket.state(),
                    State::Established | State::FinWait1 | State::FinWait2
                ))
                .then_some(Ok(0)),
            }
        })
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.smol.run(|stack| {
            let socket = stack.sockets.get_mut::<tcp::Socket>(self.handle);
            match socket.send_slice(buf) {
                Ok(0) => None,
                Ok(n) => {
                    // Send now rather than at the next poll.
                    stack.poll();
                    Some(Ok(n))
                }
                Err(_) if !socket.may_send() => Some(Err(Cow::Borrowed("connection closed"))),
                Err(_) => None,
            }
        })
    }

    fn local(&self) -> String {
        self.local.clone()
    }

    fn remote(&self) -> String {
        self.remote.clone()
    }

    fn hangup(&self) {
        let mut stack = self.smol.lock();
        stack.sockets.get_mut::<tcp::Socket>(self.handle).close();
        stack.poll();
    }
}

struct SmolListen {
    smol: Arc<Smol>,
    /// The socket listening now; an accepted one is replaced.
    handle: Mutex<SocketHandle>,
    port: u16,
    local: String,
}

impl Listener for SmolListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        let (handle, local, remote) = self.smol.run(|stack| {
            let handle = *self.handle.lock();
            let socket = stack.sockets.get::<tcp::Socket>(handle);
            if socket.state() != State::Established {
                return None;
            }
            let local = endpoint(socket.local_endpoint());
            let remote = endpoint(socket.remote_endpoint());
            let born = stack.alloc();
            if stack
                .sockets
                .get_mut::<tcp::Socket>(born)
                .listen(self.port)
                .is_err()
            {
                stack.free.push(born);
                return Some(Err(Cow::Borrowed("address in use")));
            }
            *self.handle.lock() = born;
            Some(Ok((handle, local, remote)))
        })?;
        Ok(Arc::new(TcpConn {
            smol: Arc::clone(&self.smol),
            handle,
            local,
            remote,
        }))
    }

    fn local(&self) -> String {
        self.local.clone()
    }
}

/// The loopback UDP ports in use, by number.
type LoopUdpPorts = Mutex<Vec<(u16, Weak<LoopUdp>)>>;

/// In-process connections for `127.0.0.1`. The guest tests dial themselves.
struct Loopback {
    listeners: Mutex<Vec<(u16, Arc<LoopListen>)>>,
    udp: Arc<LoopUdpPorts>,
    next: AtomicU16,
}

impl Loopback {
    fn new() -> Loopback {
        Loopback {
            listeners: Mutex::new(Vec::new()),
            udp: Arc::new(Mutex::new(Vec::new())),
            next: AtomicU16::new(1),
        }
    }

    /// Opens loopback UDP port `port` (0: a fresh one).
    fn udp_port(&self, port: u16, announced: bool) -> Result<Arc<LoopUdp>> {
        let mut ports = self.udp.lock();
        ports.retain(|(_, w)| w.strong_count() > 0);
        let port = if port == 0 {
            loop {
                let p = self.port();
                if ports.iter().all(|(q, _)| *q != p) {
                    break p;
                }
            }
        } else if ports.iter().any(|(q, _)| *q == port) {
            return Err(Cow::Borrowed("address in use"));
        } else {
            port
        };
        let sock = Arc::new(LoopUdp {
            port,
            demux: Mutex::new(UdpDemux::new(announced)),
            ready: Rendez::new(),
        });
        ports.push((port, Arc::downgrade(&sock)));
        Ok(sock)
    }

    fn dial_udp(&self, port: u16) -> Result<Arc<dyn Conn>> {
        let sock = self.udp_port(0, false)?;
        let id = sock.demux.lock().connect(port);
        Ok(Arc::new(LoopUdpConn {
            ports: Arc::clone(&self.udp),
            sock,
            id,
            remote: port,
            closed: AtomicBool::new(false),
        }))
    }

    fn announce_udp(&self, port: u16) -> Result<Arc<dyn Listener>> {
        Ok(Arc::new(LoopUdpListen {
            ports: Arc::clone(&self.udp),
            sock: self.udp_port(port, true)?,
        }))
    }

    fn port(&self) -> u16 {
        self.next.fetch_add(1, Ordering::Relaxed).max(1)
    }

    fn announce(&self, port: u16) -> Result<Arc<dyn Listener>> {
        let port = if port == 0 { self.port() } else { port };
        let listen = Arc::new(LoopListen {
            port,
            waiting: Mutex::new(VecDeque::new()),
            ready: Rendez::new(),
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
        let local_port = self.port();
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
        listen.ready.wakeup();
        Ok(client)
    }
}

struct LoopListen {
    port: u16,
    waiting: Mutex<VecDeque<Arc<LoopConn>>>,
    ready: Rendez,
}

impl Listener for LoopListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            if let Some(conn) = self.waiting.lock().pop_front() {
                return Ok(conn);
            }
            sleep(&self.ready, || !self.waiting.lock().is_empty(), None)?;
        }
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.port)
    }
}

/// One direction of a loopback connection: bytes for its reader, and
/// whether its writer's end hung up.
struct Pipe {
    q: Mutex<VecDeque<u8>>,
    /// Set by the hangup of the connection that reads this pipe.
    closed: AtomicBool,
    ready: Rendez,
}

impl Pipe {
    fn new() -> Pipe {
        Pipe {
            q: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            ready: Rendez::new(),
        }
    }
}

struct LoopConn {
    /// What this end reads.
    mine: Arc<Pipe>,
    /// What this end writes: the other end's `mine`.
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
                    for (byte, b) in buf[..n].iter_mut().zip(q.drain(..n)) {
                        *byte = b;
                    }
                    return Ok(n);
                }
                if self.peer.closed.load(Ordering::Acquire) {
                    return Ok(0);
                }
            }
            sleep(
                &self.mine.ready,
                || !self.mine.q.lock().is_empty() || self.peer.closed.load(Ordering::Acquire),
                None,
            )?;
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if self.peer.closed.load(Ordering::Acquire) {
            return Err(Cow::Borrowed("connection closed"));
        }
        self.peer.q.lock().extend(buf.iter().copied());
        self.peer.ready.wakeup();
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
        // The other end reads `peer` and watches `mine.closed`.
        self.peer.ready.wakeup();
    }
}

/// A loopback UDP port and the conversations on it.
struct LoopUdp {
    port: u16,
    demux: Mutex<UdpDemux<u16>>,
    /// Woken when a datagram arrives or a conversation hangs up.
    ready: Rendez,
}

struct LoopUdpListen {
    ports: Arc<LoopUdpPorts>,
    sock: Arc<LoopUdp>,
}

impl Listener for LoopUdpListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            if let Some((id, remote)) = self.sock.demux.lock().accept() {
                return Ok(Arc::new(LoopUdpConn {
                    ports: Arc::clone(&self.ports),
                    sock: Arc::clone(&self.sock),
                    id,
                    remote,
                    closed: AtomicBool::new(false),
                }));
            }
            sleep(&self.sock.ready, || self.sock.demux.lock().pending(), None)?;
        }
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.sock.port)
    }
}

struct LoopUdpConn {
    ports: Arc<LoopUdpPorts>,
    sock: Arc<LoopUdp>,
    id: u64,
    remote: u16,
    closed: AtomicBool,
}

impl Conn for LoopUdpConn {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            if let Some(n) = self.sock.demux.lock().recv(self.id, buf) {
                return Ok(n);
            }
            sleep(
                &self.sock.ready,
                || self.closed.load(Ordering::Acquire) || self.sock.demux.lock().ready(self.id),
                None,
            )?;
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Cow::Borrowed("connection closed"));
        }
        if buf.len() > UDP_MAX {
            return Err(Cow::Borrowed("message too long"));
        }
        let peer = self
            .ports
            .lock()
            .iter()
            .find(|(p, _)| *p == self.remote)
            .and_then(|(_, w)| w.upgrade());
        // A datagram nobody is listening for is lost, as on a wire.
        if let Some(peer) = peer
            && peer.demux.lock().deliver(self.sock.port, buf.to_vec())
        {
            peer.ready.wakeup();
        }
        Ok(buf.len())
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.sock.port)
    }

    fn remote(&self) -> String {
        format!("127.0.0.1!{}", self.remote)
    }

    fn hangup(&self) {
        self.closed.store(true, Ordering::Release);
        self.sock.demux.lock().forget(self.id);
        self.sock.ready.wakeup();
    }
}
