//! A [`Net`] for `#I` on bare metal: a loopback interface for 127/8, and
//! `smoltcp` over a [`VirtioNet`] for everything else.
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
//! The loopback is Inferno's `bind loopback /dev/null` with address
//! `127.0.0.1/8`: `/net/ipifc` lists it after the card's interface (or
//! alone), with its routes in `/net/iproute`. It carries TCP and UDP
//! between progs in process ([`Loopback`]). An announce on `*` listens on
//! the loopback and on the card, on one port, so a dial to `127.0.0.1`
//! reaches it; an announce on the card's address stays on the card.
//! Nothing for 127/8 is ever sent on the card, whatever `/net/iproute`
//! says, and a dial to an address on 127/8 other than `127.0.0.1` gets
//! nowhere.
//!
//! Waiting: every operation polls the interface, and if it cannot finish
//! sleeps until the card interrupts (a frame arrived or was sent) or
//! [`MAX_WAIT_MS`] passes, so TCP timers still run. A killed prog's sleep
//! ends with `interrupted`, as with `purg_core::block::park`.
//!
//! The interface has one IPv4 address and a default route ([`Ipv4Config`]),
//! static at boot; `/net/ipifc/0/ctl` and `/net/iproute` replace them, as
//! `ip/dhcp` does, and `/net/iproute` adds routes to other networks through
//! gateways (`ip/rip`), up to 64 in all. ARP for the gateway is smoltcp's.
//! UDP `headers` mode is one smoltcp socket whose datagrams all go to one
//! conversation; on `*` the loopback port's datagrams join them.
//!
//! UDP follows Inferno: a dialed conversation reads and writes datagrams
//! with one remote, and an announced port starts a conversation for each
//! new remote, which `listen` returns ([`UdpDemux`]). IPv4 fragmentation
//! and reassembly are smoltcp's, so a datagram may be up to [`UDP_MAX`]
//! bytes, past the MTU.
//!
//! ICMP (`/net/icmp`) is echo only. A conversation with another host is a
//! raw smoltcp socket for IPv4 ICMP: it sends the echo requests with an
//! IPv4 header it builds, and keeps the echo replies from that host that
//! carry its identifier, with their headers (TTL included). `127.0.0.1`
//! is answered in process.

use alloc::borrow::Cow;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

use purg_platform::Result;
use purg_platform::net::{self, Conn, Listener, Net, UdpDemux};
use purg_platform::sync::{Mutex, MutexGuard, Rendez};
use smoltcp::iface::{Config, Interface, Route, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{self, State};
use smoltcp::socket::{raw, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion,
    Ipv4Address,
};
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
/// The largest UDP payload: smoltcp's 8 KiB IPv4 fragmentation and
/// reassembly buffers (the `fragmentation-buffer-size-8192` and
/// `reassembly-buffer-size-8192` features) less the IP and UDP headers.
/// A datagram past the 1500-byte MTU goes out in fragments, and one that
/// came in fragments is reassembled, two at a time. Larger writes fail.
pub const UDP_MAX: usize = 8192 - 20 - 8;
/// The bytes of datagrams each UDP socket buffers each way: as many
/// datagrams of a full MTU as [`UDP_PACKETS`], or fewer larger ones.
const UDP_BUF: usize = UDP_PACKETS * (IP_MTU - 28);

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
                let mac = dev.mac();
                let stack = Stack::open(dev, cfg)?;
                IRQ_MMIO.call_once(|| t.mmio);
                WAKE.call_once(Rendez::new);
                irq::register(t.intid, card_irq);
                Ok(Arc::new(Smol {
                    inner: Mutex::new(stack),
                    cfg: Mutex::new(*cfg),
                    mac,
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
        match dest(host) {
            Dest::Loopback => self.loopback.dial(port),
            // Nothing else on 127/8 answers, and it never goes on the card.
            Dest::LoopNet(_) => Err(Cow::Borrowed("connection refused")),
            Dest::Card(ip) => self.stack()?.dial(Ipv4Address::from(ip), port),
            Dest::Any | Dest::Bad => Err(Cow::Borrowed("no translation")),
        }
    }

    fn announce_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>> {
        match dest(host) {
            Dest::Loopback | Dest::LoopNet(_) => {
                Ok(self.loopback.announce(port, false)? as Arc<dyn Listener>)
            }
            Dest::Any => self.announce_any(port),
            Dest::Card(_) => Ok(self.stack()?.listen(port)? as Arc<dyn Listener>),
            Dest::Bad => Err(Cow::Borrowed("no translation")),
        }
    }

    fn dial_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        match dest(host) {
            Dest::Loopback => self.loopback.dial_udp(LOCALHOST, port),
            Dest::LoopNet(ip) => self.loopback.dial_udp(ip, port),
            Dest::Card(ip) => self.stack()?.dial_udp(Ipv4Address::from(ip), port),
            Dest::Any | Dest::Bad => Err(Cow::Borrowed("no translation")),
        }
    }

    fn announce_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>> {
        match dest(host) {
            Dest::Loopback | Dest::LoopNet(_) => self.loopback.announce_udp(port),
            Dest::Any => self.announce_any_udp(port),
            Dest::Card(_) => self.stack()?.announce_udp(port),
            Dest::Bad => Err(Cow::Borrowed("no translation")),
        }
    }

    fn announce_udp_headers(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>> {
        match (dest(host), &self.smol) {
            (Dest::Loopback | Dest::LoopNet(_), _) | (Dest::Any, Err(_)) => {
                Ok(Arc::new(self.loopback.udp_headers(port, false)?))
            }
            (Dest::Any, Ok(smol)) => {
                let port = self.free_udp_port(smol, port);
                let lo = self.loopback.udp_headers(port, true)?;
                Ok(Arc::new(AnyUdpHeaders {
                    lo,
                    smol: smol.announce_udp_headers(port)?,
                }))
            }
            (Dest::Card(_), _) => Ok(self.stack()?.announce_udp_headers(port)? as Arc<dyn Conn>),
            (Dest::Bad, _) => Err(Cow::Borrowed("no translation")),
        }
    }

    /// The card's interface, if there is a card, then the loopback.
    fn interfaces(&self) -> Vec<net::Ifc> {
        let mut out: Vec<net::Ifc> = self.smol.iter().map(|s| s.interface()).collect();
        out.push(net::Ifc::loopback());
        out
    }

    /// Only the card's address can change; the loopback's is fixed.
    fn set_address(&self, n: usize, addr: [u8; 4], prefix: u8) -> Result<()> {
        match (&self.smol, n) {
            (Ok(smol), 0) => {
                smol.set_address(addr, prefix);
                Ok(())
            }
            (Ok(_), 1) | (Err(_), 0) => Err(Cow::Borrowed("permission denied")),
            _ => Err(Cow::Borrowed("file does not exist")),
        }
    }

    fn add_route(&self, target: [u8; 4], mask: [u8; 4], gateway: [u8; 4]) -> Result<()> {
        // Without a card there is only loopback, which routes nothing:
        // `#I` keeps the table, as on the hosted emu. With one, 127/8
        // still never leaves by it, whatever the routes say.
        self.smol
            .as_ref()
            .map_or(Ok(()), |s| s.route(target, mask, Some(gateway)))
    }

    fn remove_route(&self, target: [u8; 4], mask: [u8; 4]) -> Result<()> {
        self.smol
            .as_ref()
            .map_or(Ok(()), |s| s.route(target, mask, None))
    }

    fn dial_icmp(&self, host: &str) -> Result<Arc<dyn Conn>> {
        match dest(host) {
            Dest::Loopback => Ok(Arc::new(LoopIcmp::new(LOCALHOST))),
            Dest::LoopNet(ip) => Ok(Arc::new(LoopIcmp::new(ip))),
            Dest::Card(ip) => self.stack()?.dial_icmp(Ipv4Address::from(ip)),
            Dest::Any | Dest::Bad => Err(Cow::Borrowed("no translation")),
        }
    }

    fn resolve(&self, host: &str, _port: u16) -> Result<String> {
        if host == "*" || host.is_empty() || parse_v4(host).is_some() {
            Ok(String::from(host))
        } else {
            Err(Cow::Borrowed("no translation"))
        }
    }
}

impl NetStack {
    /// Announces TCP `port` on every address (`*`): on the loopback, and
    /// on the card if there is one, on the same port. A fresh port is one
    /// of the card's ephemeral ports that the loopback has free.
    fn announce_any(&self, port: u16) -> Result<Arc<dyn Listener>> {
        let Ok(smol) = &self.smol else {
            let lo = self.loopback.announce(port, false)?;
            return Ok(Arc::new(AnyListen { lo, smol: None }));
        };
        let lo = if port != 0 {
            self.loopback.announce(port, true)?
        } else {
            loop {
                let p = smol.lock().ephemeral();
                if let Ok(lo) = self.loopback.announce(p, true) {
                    break lo;
                }
            }
        };
        let smol = smol.listen(lo.port)?;
        Ok(Arc::new(AnyListen {
            lo,
            smol: Some(smol),
        }))
    }

    /// Announces UDP `port` on every address, as [`NetStack::announce_any`].
    fn announce_any_udp(&self, port: u16) -> Result<Arc<dyn Listener>> {
        let Ok(smol) = &self.smol else {
            let lo = self.loopback.udp_port(port, true, false)?;
            return Ok(Arc::new(AnyUdpListen {
                ports: Arc::clone(&self.loopback.udp),
                lo,
                smol: None,
                any: true,
            }));
        };
        let port = self.free_udp_port(smol, port);
        let lo = self.loopback.udp_port(port, true, true)?;
        Ok(Arc::new(AnyUdpListen {
            ports: Arc::clone(&self.loopback.udp),
            lo,
            smol: Some(smol.udp_port(port, true)?),
            any: true,
        }))
    }

    /// `port`, or if it is 0 one of the card's ephemeral ports that no
    /// loopback UDP port has.
    fn free_udp_port(&self, smol: &Smol, port: u16) -> u16 {
        if port != 0 {
            return port;
        }
        loop {
            let p = smol.lock().ephemeral();
            if self.loopback.udp_free(p) {
                return p;
            }
        }
    }
}

/// The loopback's address.
const LOCALHOST: [u8; 4] = [127, 0, 0, 1];

/// Where an address leads.
enum Dest {
    /// `127.0.0.1` (or `::1`), the loopback's own address.
    Loopback,
    /// Another address on the loopback's network, 127/8. The loopback
    /// carries packets to it, but nothing has that address, so they are
    /// lost; they are never sent on the card.
    LoopNet([u8; 4]),
    /// Every address: `*`, empty, `0.0.0.0` or `::`.
    Any,
    /// Another IPv4 address: the card's business.
    Card([u8; 4]),
    /// Not an address.
    Bad,
}

fn dest(host: &str) -> Dest {
    match host {
        "127.0.0.1" | "::1" => Dest::Loopback,
        "" | "*" | "0.0.0.0" | "::" => Dest::Any,
        _ => match parse_v4(host) {
            Some(ip) if on_loopback(ip) => Dest::LoopNet(ip),
            Some(ip) => Dest::Card(ip),
            None => Dest::Bad,
        },
    }
}

/// Whether `ip` is on the loopback's network.
fn on_loopback(ip: [u8; 4]) -> bool {
    ip[0] == 127
}

/// Tells the card's sleepers ([`Smol::run`]) to look again: a listener
/// that also waits on the loopback has a call or a datagram there.
fn kick() {
    EVENTS.fetch_add(1, Ordering::Release);
    if let Some(w) = WAKE.get() {
        w.wakeup();
    }
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
            vec![0; UDP_BUF],
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
    /// The interface's configuration, as `/net/ipifc` and `/net/iproute`
    /// last set it.
    cfg: Mutex<Ipv4Config>,
    mac: [u8; 6],
}

impl Smol {
    fn lock(&self) -> MutexGuard<'_, Stack> {
        self.inner.lock()
    }

    /// The interface's address.
    fn local_ip(&self) -> Ipv4Address {
        Ipv4Address::from(self.cfg.lock().addr)
    }

    fn interface(&self) -> net::Ifc {
        let cfg = *self.cfg.lock();
        net::Ifc {
            device: String::from("ether0"),
            mtu: IP_MTU,
            mac: self.mac,
            addr: cfg.addr,
            prefix: cfg.prefix,
            gateway: (cfg.gateway != [0; 4]).then_some(cfg.gateway),
        }
    }

    /// Replaces the interface's address. Connections already open keep
    /// the old one and fail.
    fn set_address(&self, addr: [u8; 4], prefix: u8) {
        let mut stack = self.lock();
        stack.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            addrs
                .push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::from(addr)),
                    prefix,
                ))
                .ok();
        });
        let mut cfg = self.cfg.lock();
        cfg.addr = addr;
        cfg.prefix = prefix;
    }

    /// Routes `target`/`mask` through `gateway`, or with `None` stops
    /// routing it (`/net/iproute`). smoltcp sends to the gateway of the
    /// longest prefix that holds an address off the interface's network;
    /// the default route is the interface's gateway.
    fn route(&self, target: [u8; 4], mask: [u8; 4], gateway: Option<[u8; 4]>) -> Result<()> {
        let bits = u32::from_be_bytes(mask);
        let prefix = bits.leading_ones() as u8;
        if bits.count_ones() != u32::from(prefix) {
            // smoltcp routes prefixes only.
            return Err(Cow::Borrowed("bad arg in system call"));
        }
        let cidr = IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(target)), prefix);
        let mut stack = self.lock();
        let mut full = false;
        stack.iface.routes_mut().update(|routes| {
            routes.retain(|r| r.cidr != cidr);
            if let Some(gw) = gateway {
                let route = Route {
                    cidr,
                    via_router: IpAddress::Ipv4(Ipv4Address::from(gw)),
                    preferred_until: None,
                    expires_at: None,
                };
                full = routes.push(route).is_err();
            }
        });
        if full {
            return Err(Cow::Borrowed("route table full"));
        }
        if prefix == 0 {
            self.cfg.lock().gateway = gateway.unwrap_or([0; 4]);
        }
        Ok(())
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

    fn listen(self: &Arc<Self>, port: u16) -> Result<Arc<SmolListen>> {
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
            local: format!("{}!{port}", self.local_ip()),
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
            local: format!("{}!{port}", self.local_ip()),
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

impl Smol {
    /// UDP port `port` (0: an ephemeral one) in `headers` mode.
    fn announce_udp_headers(self: &Arc<Self>, port: u16) -> Result<Arc<SmolUdpHeaders>> {
        let mut stack = self.lock();
        let port = if port == 0 { stack.ephemeral() } else { port };
        let mut socket = new_udp_socket();
        if socket.bind(port).is_err() {
            return Err(Cow::Borrowed("address in use"));
        }
        let handle = stack.sockets.add(socket);
        Ok(Arc::new(SmolUdpHeaders {
            smol: Arc::clone(self),
            handle,
            port,
        }))
    }
}

/// A smoltcp UDP socket in `headers` mode: every datagram, with a header
/// naming its sender; each write names its destination, which may be a
/// broadcast address.
struct SmolUdpHeaders {
    smol: Arc<Smol>,
    handle: SocketHandle,
    port: u16,
}

impl SmolUdpHeaders {
    /// The next datagram the socket has, with its header, if any.
    fn try_read(&self, stack: &mut Stack, buf: &mut [u8]) -> Option<usize> {
        let socket = stack.sockets.get_mut::<udp::Socket>(self.handle);
        let (data, meta) = socket.recv().ok()?;
        let IpAddress::Ipv4(from) = meta.endpoint.addr;
        let to = match meta.local_address {
            Some(IpAddress::Ipv4(a)) => a,
            _ => self.smol.local_ip(),
        };
        let mut whole =
            net::udp_header(from.octets(), meta.endpoint.port, to.octets(), self.port).to_vec();
        whole.extend_from_slice(data);
        let n = whole.len().min(buf.len());
        buf[..n].copy_from_slice(&whole[..n]);
        Some(n)
    }
}

impl Conn for SmolUdpHeaders {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.smol.run(|stack| self.try_read(stack, buf).map(Ok))
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let (addr, port) = net::udp_header_remote(buf).ok_or(Cow::Borrowed("bad udp header"))?;
        let data = &buf[net::UDP_HEADER..];
        if data.len() > UDP_MAX {
            return Err(Cow::Borrowed("message too long"));
        }
        // 127/8 is the loopback's, never the card's: lost from here.
        if on_loopback(addr) {
            return Ok(buf.len());
        }
        let to = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from(addr)), port);
        self.smol.run(|stack| {
            let socket = stack.sockets.get_mut::<udp::Socket>(self.handle);
            match socket.send_slice(data, to) {
                Ok(()) => {
                    stack.poll();
                    Some(Ok(buf.len()))
                }
                Err(udp::SendError::BufferFull) => None,
                Err(udp::SendError::Unaddressable) => Some(Err(Cow::Borrowed("no route"))),
            }
        })
    }

    fn local(&self) -> String {
        format!("{}!{}", self.smol.local_ip(), self.port)
    }

    fn remote(&self) -> String {
        String::new()
    }

    fn hangup(&self) {}
}

impl Drop for SmolUdpHeaders {
    fn drop(&mut self) {
        self.smol.lock().sockets.remove(self.handle);
    }
}

/// Identifiers for ICMP echo conversations.
static ICMP_IDENT: AtomicU16 = AtomicU16::new(1);
/// The TTL echo requests are sent with, and loopback replies carry.
const ICMP_TTL: u8 = 64;
/// Packets each ICMP socket buffers each way.
const ICMP_PACKETS: usize = 8;
/// The largest IPv4 packet: the MTU, with no fragmentation.
const IP_MTU: usize = 1500;

impl Smol {
    /// An ICMP echo conversation with `ip` on a raw IPv4 socket for ICMP:
    /// it sees every ICMP packet the interface receives (smoltcp still
    /// answers echo requests itself) and keeps the echo replies from
    /// `ip` that carry this conversation's identifier.
    fn dial_icmp(self: &Arc<Self>, ip: Ipv4Address) -> Result<Arc<dyn Conn>> {
        let buf = || {
            raw::PacketBuffer::new(
                vec![raw::PacketMetadata::EMPTY; ICMP_PACKETS],
                vec![0; ICMP_PACKETS * IP_MTU],
            )
        };
        let socket = raw::Socket::new(Some(IpVersion::Ipv4), Some(IpProtocol::Icmp), buf(), buf());
        let handle = self.lock().sockets.add(socket);
        Ok(Arc::new(SmolIcmp {
            smol: Arc::clone(self),
            handle,
            remote: ip,
            ident: ICMP_IDENT.fetch_add(1, Ordering::Relaxed),
        }))
    }
}

struct SmolIcmp {
    smol: Arc<Smol>,
    handle: SocketHandle,
    remote: Ipv4Address,
    ident: u16,
}

impl SmolIcmp {
    /// The reply in the IPv4 packet `pkt`, as `#I` returns it, if it is
    /// an echo reply to this conversation.
    fn reply(&self, pkt: &[u8]) -> Option<Vec<u8>> {
        let ihl = usize::from(pkt.first()? & 0x0f) * 4;
        if pkt.len() < ihl + 8 || ihl < net::IPV4_HEADER || pkt[9] != net::PROTO_ICMP {
            return None;
        }
        let src: [u8; 4] = pkt[12..16].try_into().ok()?;
        let dst: [u8; 4] = pkt[16..20].try_into().ok()?;
        let msg = &pkt[ihl..];
        if src != self.remote.octets()
            || msg[0] != net::ICMP_ECHO_REPLY
            || u16::from_be_bytes([msg[4], msg[5]]) != self.ident
        {
            return None;
        }
        let mut out = net::ipv4_header(src, dst, pkt[8], msg.len()).to_vec();
        out.extend_from_slice(msg);
        Some(out)
    }
}

impl Conn for SmolIcmp {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.smol.run(|stack| {
            let socket = stack.sockets.get_mut::<raw::Socket>(self.handle);
            while let Ok(pkt) = socket.recv() {
                if let Some(r) = self.reply(pkt) {
                    let n = r.len().min(buf.len());
                    buf[..n].copy_from_slice(&r[..n]);
                    return Some(Ok(n));
                }
            }
            None
        })
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if buf.len() + net::IPV4_HEADER > IP_MTU {
            return Err(Cow::Borrowed("message too long"));
        }
        let mut msg = buf.to_vec();
        if msg.len() >= 8 {
            msg[4..6].copy_from_slice(&self.ident.to_be_bytes());
        }
        net::icmp_fill_checksum(&mut msg);
        let mut pkt = net::ipv4_header(
            self.smol.local_ip().octets(),
            self.remote.octets(),
            ICMP_TTL,
            msg.len(),
        )
        .to_vec();
        pkt.extend_from_slice(&msg);
        self.smol.run(|stack| {
            let socket = stack.sockets.get_mut::<raw::Socket>(self.handle);
            match socket.send_slice(&pkt) {
                Ok(()) => {
                    stack.poll();
                    Some(Ok(buf.len()))
                }
                Err(raw::SendError::BufferFull) => None,
            }
        })
    }

    fn local(&self) -> String {
        format!("{}!{}", self.smol.local_ip(), self.ident)
    }

    fn remote(&self) -> String {
        format!("{}!1", self.remote)
    }

    fn hangup(&self) {}
}

impl Drop for SmolIcmp {
    fn drop(&mut self) {
        self.smol.lock().sockets.remove(self.handle);
    }
}

/// An ICMP echo conversation on the loopback: each echo request written
/// to `127.0.0.1` is answered at once, in process, as a host's loopback
/// would. Requests to another address on 127/8 are lost, since nothing
/// there has it.
struct LoopIcmp {
    replies: Mutex<VecDeque<Vec<u8>>>,
    ready: Rendez,
    ident: u16,
    remote: [u8; 4],
}

impl LoopIcmp {
    fn new(remote: [u8; 4]) -> LoopIcmp {
        LoopIcmp {
            replies: Mutex::new(VecDeque::new()),
            ready: Rendez::new(),
            ident: ICMP_IDENT.fetch_add(1, Ordering::Relaxed),
            remote,
        }
    }
}

impl Conn for LoopIcmp {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if let Some(r) = self.replies.lock().pop_front() {
                let n = r.len().min(buf.len());
                buf[..n].copy_from_slice(&r[..n]);
                return Ok(n);
            }
            sleep(&self.ready, || !self.replies.lock().is_empty(), None)?;
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        if self.remote != LOCALHOST {
            return Ok(buf.len());
        }
        let mut msg = buf.to_vec();
        if msg.len() >= 8 {
            msg[4..6].copy_from_slice(&self.ident.to_be_bytes());
        }
        net::icmp_fill_checksum(&mut msg);
        if let Some(reply) = net::icmp_echo_reply(&msg) {
            let lo = LOCALHOST;
            let mut r = net::ipv4_header(lo, lo, ICMP_TTL, reply.len()).to_vec();
            r.extend_from_slice(&reply);
            let mut q = self.replies.lock();
            if q.len() < UDP_PACKETS {
                q.push_back(r);
            }
            drop(q);
            self.ready.wakeup();
        }
        Ok(buf.len())
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.ident)
    }

    fn remote(&self) -> String {
        format!("{}!1", Ipv4Address::from(self.remote))
    }

    fn hangup(&self) {}
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

/// A dropped connection gives its socket back. It is closed, not
/// aborted, so what was written is still sent, and it goes to the back of
/// the free list, so it is reused (and only then aborted, if the close
/// has not finished) as late as possible.
impl Drop for TcpConn {
    fn drop(&mut self) {
        let mut stack = self.smol.lock();
        stack.sockets.get_mut::<tcp::Socket>(self.handle).close();
        stack.poll();
        stack.free.insert(0, self.handle);
    }
}

struct SmolListen {
    smol: Arc<Smol>,
    /// The socket listening now; an accepted one is replaced.
    handle: Mutex<SocketHandle>,
    port: u16,
    local: String,
}

impl SmolListen {
    /// The connection the listening socket has, if it has one yet, with
    /// a new socket listening in its place.
    fn try_accept(&self, stack: &mut Stack) -> Option<Result<Arc<dyn Conn>>> {
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
        Some(Ok(Arc::new(TcpConn {
            smol: Arc::clone(&self.smol),
            handle,
            local,
            remote,
        })))
    }
}

impl Listener for SmolListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        self.smol.run(|stack| self.try_accept(stack))
    }

    fn local(&self) -> String {
        self.local.clone()
    }
}

/// A dropped listener stops listening and gives its socket back.
impl Drop for SmolListen {
    fn drop(&mut self) {
        let handle = *self.handle.lock();
        let mut stack = self.smol.lock();
        stack.sockets.get_mut::<tcp::Socket>(handle).abort();
        stack.poll();
        stack.free.insert(0, handle);
    }
}

/// The loopback UDP ports in use, by number.
type LoopUdpPorts = Mutex<Vec<(u16, Weak<LoopUdp>)>>;

/// The loopback interface, `127.0.0.1/8`: in-process connections and
/// datagrams between progs, with or without a card. Its own address is
/// `127.0.0.1`; what goes to another address on 127/8 is lost (a TCP dial
/// is refused at once). Nothing on 127/8 goes on the card.
///
/// A listener on every address (`*`) has a loopback listener too
/// ([`AnyListen`], [`AnyUdpListen`]); those are `shared`, and a call or
/// datagram for one also wakes the card's sleepers ([`kick`]), since its
/// accept waits on both.
struct Loopback {
    /// Weak: a listener lives as long as its `#I` conversation.
    listeners: Mutex<Vec<(u16, Weak<LoopListen>)>>,
    udp: Arc<LoopUdpPorts>,
    next: AtomicU16,
    /// Fresh listening ports handed out, for the next one in the
    /// ephemeral range; kept apart from `next`, which every dial takes
    /// from, so that a port comes round again only after some sixteen
    /// thousand listeners.
    listens: AtomicU32,
}

impl Loopback {
    fn new() -> Loopback {
        Loopback {
            listeners: Mutex::new(Vec::new()),
            udp: Arc::new(Mutex::new(Vec::new())),
            next: AtomicU16::new(1),
            listens: AtomicU32::new(0),
        }
    }

    /// Whether no loopback UDP port is `port`.
    fn udp_free(&self, port: u16) -> bool {
        let mut ports = self.udp.lock();
        ports.retain(|(_, w)| w.strong_count() > 0);
        ports.iter().all(|(q, _)| *q != port)
    }

    /// Opens loopback UDP port `port` (0: a fresh one).
    fn udp_port(&self, port: u16, announced: bool, shared: bool) -> Result<Arc<LoopUdp>> {
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
            shared,
        });
        ports.push((port, Arc::downgrade(&sock)));
        Ok(sock)
    }

    /// Loopback UDP port `port` in `headers` mode.
    fn udp_headers(&self, port: u16, shared: bool) -> Result<LoopUdpHeaders> {
        Ok(LoopUdpHeaders {
            ports: Arc::clone(&self.udp),
            sock: self.udp_port(port, true, shared)?,
        })
    }

    /// A UDP conversation with `ip`!`port`, on 127/8.
    fn dial_udp(&self, ip: [u8; 4], port: u16) -> Result<Arc<dyn Conn>> {
        let sock = self.udp_port(0, false, false)?;
        let id = sock.demux.lock().connect(port);
        Ok(Arc::new(LoopUdpConn {
            ports: Arc::clone(&self.udp),
            sock,
            id,
            remote: port,
            remote_ip: ip,
            closed: AtomicBool::new(false),
        }))
    }

    fn announce_udp(&self, port: u16) -> Result<Arc<dyn Listener>> {
        Ok(Arc::new(AnyUdpListen {
            ports: Arc::clone(&self.udp),
            lo: self.udp_port(port, true, false)?,
            smol: None,
            any: false,
        }))
    }

    fn port(&self) -> u16 {
        self.next.fetch_add(1, Ordering::Relaxed).max(1)
    }

    /// A fresh listening port: the ephemeral range, round and round.
    fn listen_port(&self) -> u16 {
        let range = u32::from(u16::MAX - FIRST_EPHEMERAL) + 1;
        let n = self.listens.fetch_add(1, Ordering::Relaxed) % range;
        FIRST_EPHEMERAL + n as u16
    }

    /// Listens on loopback TCP port `port` (0: a fresh one). A fresh
    /// port comes from the ephemeral range and skips ports with a live
    /// listener, and a port that has one is `address in use`: two
    /// listeners on one port would each get the other's calls. Before,
    /// fresh ports came from the counter dials use, from 1 up; it wrapped
    /// often enough under load that a dial to a server that had gone
    /// reached another prog's.
    fn announce(&self, port: u16, shared: bool) -> Result<Arc<LoopListen>> {
        let mut listeners = self.listeners.lock();
        listeners.retain(|(_, w)| w.strong_count() > 0);
        let used = |p: u16| listeners.iter().any(|(q, _)| *q == p);
        let port = if port == 0 {
            loop {
                let p = self.listen_port();
                if !used(p) {
                    break p;
                }
            }
        } else if used(port) {
            return Err(Cow::Borrowed("address in use"));
        } else {
            port
        };
        let listen = Arc::new(LoopListen {
            port,
            waiting: Mutex::new(VecDeque::new()),
            ready: Rendez::new(),
            shared,
        });
        listeners.push((port, Arc::downgrade(&listen)));
        Ok(listen)
    }

    fn dial(&self, port: u16) -> Result<Arc<dyn Conn>> {
        let listen = self
            .listeners
            .lock()
            .iter()
            .filter(|(p, _)| *p == port)
            .find_map(|(_, l)| l.upgrade())
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
        if listen.shared {
            kick();
        }
        Ok(client)
    }
}

struct LoopListen {
    port: u16,
    waiting: Mutex<VecDeque<Arc<LoopConn>>>,
    ready: Rendez,
    /// Part of an [`AnyListen`] that also listens on the card.
    shared: bool,
}

impl LoopListen {
    fn calling(&self) -> bool {
        !self.waiting.lock().is_empty()
    }
}

impl Listener for LoopListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            if let Some(conn) = self.waiting.lock().pop_front() {
                return Ok(conn);
            }
            sleep(&self.ready, || self.calling(), None)?;
        }
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.port)
    }
}

/// A TCP listener on every address: the loopback's, and the card's if
/// there is one, on one port.
struct AnyListen {
    lo: Arc<LoopListen>,
    smol: Option<Arc<SmolListen>>,
}

impl Listener for AnyListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            if let Some(conn) = self.lo.waiting.lock().pop_front() {
                return Ok(conn);
            }
            match &self.smol {
                None => {
                    sleep(&self.lo.ready, || self.lo.calling(), None)?;
                }
                // A loopback call kicks the card's sleepers, so one wait
                // covers both.
                Some(s) => {
                    let conn = s.smol.run(|stack| {
                        if self.lo.calling() {
                            return Some(Ok(None));
                        }
                        s.try_accept(stack).map(|r| r.map(Some))
                    })?;
                    if let Some(conn) = conn {
                        return Ok(conn);
                    }
                }
            }
        }
    }

    /// Every address, as the hosted emu shows it.
    fn local(&self) -> String {
        format!("*!{}", self.lo.port)
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
    /// Part of an [`AnyUdpListen`] or [`AnyUdpHeaders`] that also
    /// listens on the card.
    shared: bool,
}

/// Sends `data` from loopback UDP port `from` to `127.0.0.1!to`. A
/// datagram nobody is listening for is lost, as on a wire.
fn loop_send(ports: &LoopUdpPorts, from: u16, to: u16, data: &[u8]) {
    let peer = ports
        .lock()
        .iter()
        .find(|(p, _)| *p == to)
        .and_then(|(_, w)| w.upgrade());
    if let Some(peer) = peer
        && peer.demux.lock().deliver(from, data.to_vec())
    {
        peer.ready.wakeup();
        if peer.shared {
            kick();
        }
    }
}

/// A UDP port announced on every address: the loopback's, and the card's
/// if there is one, on one port number. A new remote on either is a new
/// conversation from `listen`.
struct AnyUdpListen {
    ports: Arc<LoopUdpPorts>,
    lo: Arc<LoopUdp>,
    smol: Option<Arc<SmolUdp>>,
    /// Announced on `*`, not on `127.0.0.1` alone.
    any: bool,
}

impl AnyUdpListen {
    fn lo_conn(&self, id: u64, remote: u16) -> Arc<dyn Conn> {
        Arc::new(LoopUdpConn {
            ports: Arc::clone(&self.ports),
            sock: Arc::clone(&self.lo),
            id,
            remote,
            remote_ip: LOCALHOST,
            closed: AtomicBool::new(false),
        })
    }
}

impl Listener for AnyUdpListen {
    fn accept(&self) -> Result<Arc<dyn Conn>> {
        loop {
            if let Some((id, remote)) = self.lo.demux.lock().accept() {
                return Ok(self.lo_conn(id, remote));
            }
            match &self.smol {
                None => {
                    sleep(&self.lo.ready, || self.lo.demux.lock().pending(), None)?;
                }
                Some(s) => {
                    let got = s.smol.run(|stack| {
                        if self.lo.demux.lock().pending() {
                            return Some(Ok(None));
                        }
                        s.pump(stack);
                        s.demux.lock().accept().map(|c| Ok(Some(c)))
                    })?;
                    if let Some((id, remote)) = got {
                        return Ok(Arc::new(SmolUdpConn::new(Arc::clone(s), id, remote, false)));
                    }
                }
            }
        }
    }

    /// Every address, as the hosted emu shows it, or the loopback's.
    fn local(&self) -> String {
        let host = if self.any { "*" } else { "127.0.0.1" };
        format!("{host}!{}", self.lo.port)
    }
}

struct LoopUdpConn {
    ports: Arc<LoopUdpPorts>,
    sock: Arc<LoopUdp>,
    id: u64,
    remote: u16,
    /// The remote's address: `127.0.0.1`, or another on 127/8, to which
    /// datagrams are lost.
    remote_ip: [u8; 4],
    closed: AtomicBool,
}

/// A loopback UDP port in `headers` mode.
struct LoopUdpHeaders {
    ports: Arc<LoopUdpPorts>,
    sock: Arc<LoopUdp>,
}

impl LoopUdpHeaders {
    /// The next datagram, with its header, if one has come.
    fn try_read(&self, buf: &mut [u8]) -> Option<usize> {
        if !self.sock.demux.lock().any_ready() {
            return None;
        }
        let mut data = vec![0u8; UDP_MAX];
        let (n, from) = self.sock.demux.lock().recv_any(&mut data)?;
        let mut whole = net::udp_header(LOCALHOST, from, LOCALHOST, self.sock.port).to_vec();
        whole.extend_from_slice(&data[..n]);
        let k = whole.len().min(buf.len());
        buf[..k].copy_from_slice(&whole[..k]);
        Some(k)
    }
}

impl Conn for LoopUdpHeaders {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if let Some(n) = self.try_read(buf) {
                return Ok(n);
            }
            sleep(
                &self.sock.ready,
                || self.sock.demux.lock().any_ready(),
                None,
            )?;
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let (addr, port) = net::udp_header_remote(buf).ok_or(Cow::Borrowed("bad udp header"))?;
        let data = &buf[net::UDP_HEADER..];
        if data.len() > UDP_MAX {
            return Err(Cow::Borrowed("message too long"));
        }
        // Loopback reaches only loopback; anything else is lost.
        if addr == LOCALHOST {
            loop_send(&self.ports, self.sock.port, port, data);
        }
        Ok(buf.len())
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.sock.port)
    }

    fn remote(&self) -> String {
        String::new()
    }

    fn hangup(&self) {}
}

/// A UDP port in `headers` mode on every address: datagrams from the
/// loopback and the card, and each write goes out by the one its
/// destination is on.
struct AnyUdpHeaders {
    lo: LoopUdpHeaders,
    smol: Arc<SmolUdpHeaders>,
}

impl Conn for AnyUdpHeaders {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        // A loopback datagram kicks the card's sleepers.
        self.smol.smol.run(|stack| {
            if let Some(n) = self.lo.try_read(buf) {
                return Some(Ok(n));
            }
            self.smol.try_read(stack, buf).map(Ok)
        })
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        match net::udp_header_remote(buf) {
            Some((addr, _)) if on_loopback(addr) => self.lo.write(buf),
            _ => self.smol.write(buf),
        }
    }

    fn local(&self) -> String {
        format!("*!{}", self.lo.sock.port)
    }

    fn remote(&self) -> String {
        String::new()
    }

    fn hangup(&self) {}
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
        if self.remote_ip == LOCALHOST {
            loop_send(&self.ports, self.sock.port, self.remote, buf);
        }
        Ok(buf.len())
    }

    fn local(&self) -> String {
        format!("127.0.0.1!{}", self.sock.port)
    }

    fn remote(&self) -> String {
        format!("{}!{}", Ipv4Address::from(self.remote_ip), self.remote)
    }

    fn hangup(&self) {
        self.closed.store(true, Ordering::Release);
        self.sock.demux.lock().forget(self.id);
        self.sock.ready.wakeup();
    }
}
