//! Byte streams the IP device (`#I`) and the command device (`#C`) need.
//! The hosted emu implements them with sockets and processes. The probe
//! and the kernel implement them with smoltcp. Both sit behind these traits
//! so `purg-core` stays `no_std`.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::Result;

/// A connected byte stream: one TCP connection or one running command's
/// standard stream.
pub trait Conn: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> Result<usize>;
    fn write(&self, buf: &[u8]) -> Result<usize>;
    fn local(&self) -> String;
    fn remote(&self) -> String;
    fn hangup(&self);
}

/// A TCP listener. [`Listener::accept`] blocks until a connection arrives.
pub trait Listener: Send + Sync {
    fn accept(&self) -> Result<Arc<dyn Conn>>;
    fn local(&self) -> String;
}

/// What `#I` asks of the platform.
pub trait Net: Send + Sync {
    fn dial_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>>;
    fn announce_tcp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>>;
    /// A UDP conversation with one remote: each read returns one datagram
    /// from it (datagrams from others are dropped) and each write sends one.
    fn dial_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>>;
    /// An announced UDP port. As in Inferno, the first datagram from each
    /// new remote starts a conversation, which [`Listener::accept`]
    /// returns; it then behaves like a dialed one to that remote.
    fn announce_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Listener>>;
    /// Resolves `host` for `port` to a numeric address (`127.0.0.1`).
    /// `*` and a literal address are returned unchanged.
    fn resolve(&self, host: &str, port: u16) -> Result<String>;
}

/// A host command started by `#C`.
pub trait Command: Send + Sync {
    fn read_stdout(&self, buf: &mut [u8]) -> Result<usize>;
    fn read_stderr(&self, buf: &mut [u8]) -> Result<usize>;
    fn write_stdin(&self, buf: &[u8]) -> Result<usize>;
    /// Blocks until the command exits. The string is its status (`""` or
    /// `"exit N"`).
    fn wait(&self) -> Result<String>;
    fn status(&self) -> String;
}

/// The most datagrams a UDP conversation holds unread; later ones are
/// dropped, as by a full socket buffer.
pub const UDP_QUEUE: usize = 64;

struct Peer<E> {
    id: u64,
    remote: E,
    accepted: bool,
    queue: VecDeque<Vec<u8>>,
}

/// Sorts the datagrams arriving on one UDP port into conversations, one per
/// remote endpoint `E`, with Inferno's semantics (see [`Net::announce_udp`]).
/// It takes no lock: the platform keeps it beside its socket, under the
/// socket's lock, and wakes readers itself.
pub struct UdpDemux<E> {
    announced: bool,
    peers: Vec<Peer<E>>,
    next: u64,
}

impl<E: PartialEq + Clone> UdpDemux<E> {
    /// A demux for an announced port (`true`), where new remotes start
    /// conversations, or for a dialed one, which only remotes added with
    /// [`UdpDemux::connect`] reach.
    pub fn new(announced: bool) -> UdpDemux<E> {
        UdpDemux {
            announced,
            peers: Vec::new(),
            next: 0,
        }
    }

    fn add(&mut self, remote: E, accepted: bool) -> usize {
        let id = self.next;
        self.next += 1;
        self.peers.push(Peer {
            id,
            remote,
            accepted,
            queue: VecDeque::new(),
        });
        self.peers.len() - 1
    }

    /// Starts the conversation with `remote`, returning its id.
    pub fn connect(&mut self, remote: E) -> u64 {
        let i = self.add(remote, true);
        self.peers[i].id
    }

    /// Files one datagram from `from`. Returns whether it was kept.
    pub fn deliver(&mut self, from: E, data: Vec<u8>) -> bool {
        let i = match self.peers.iter().position(|p| p.remote == from) {
            Some(i) => i,
            None if self.announced => self.add(from, false),
            None => return false,
        };
        let queue = &mut self.peers[i].queue;
        if queue.len() >= UDP_QUEUE {
            return false;
        }
        queue.push_back(data);
        true
    }

    /// Whether [`UdpDemux::accept`] would return a conversation.
    pub fn pending(&self) -> bool {
        self.peers.iter().any(|p| !p.accepted)
    }

    /// The oldest conversation not yet accepted: its id and remote.
    pub fn accept(&mut self) -> Option<(u64, E)> {
        let p = self.peers.iter_mut().find(|p| !p.accepted)?;
        p.accepted = true;
        Some((p.id, p.remote.clone()))
    }

    /// Whether conversation `id` has a datagram to read.
    pub fn ready(&self, id: u64) -> bool {
        self.peers.iter().any(|p| p.id == id && !p.queue.is_empty())
    }

    /// Reads conversation `id`'s next datagram into `buf`, truncated to
    /// fit. `None` if there is none yet.
    pub fn recv(&mut self, id: u64, buf: &mut [u8]) -> Option<usize> {
        let peer = self.peers.iter_mut().find(|p| p.id == id)?;
        let data = peer.queue.pop_front()?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Some(n)
    }

    /// Ends conversation `id`. On an announced port, a later datagram from
    /// its remote starts a new one.
    pub fn forget(&mut self, id: u64) {
        self.peers.retain(|p| p.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn announced_port_starts_a_conversation_per_remote() {
        let mut d = UdpDemux::new(true);
        assert!(!d.pending());
        assert!(d.deliver(1u16, vec![b'a']));
        assert!(d.deliver(2, vec![b'b']));
        assert!(d.deliver(1, vec![b'c']));
        let (one, r1) = d.accept().unwrap();
        let (two, r2) = d.accept().unwrap();
        assert_eq!((r1, r2), (1, 2));
        assert!(d.accept().is_none());
        let mut buf = [0u8; 4];
        assert_eq!(d.recv(one, &mut buf), Some(1));
        assert_eq!(buf[0], b'a');
        assert_eq!(d.recv(one, &mut buf), Some(1));
        assert_eq!(buf[0], b'c');
        assert_eq!(d.recv(one, &mut buf), None);
        assert!(d.ready(two));
        assert_eq!(d.recv(two, &mut buf), Some(1));
        assert_eq!(buf[0], b'b');
        assert!(!d.ready(two));
    }

    #[test]
    fn dialed_port_drops_strangers() {
        let mut d = UdpDemux::new(false);
        let id = d.connect(7u16);
        assert!(!d.deliver(8, vec![1]));
        assert!(!d.pending());
        assert!(d.deliver(7, vec![1, 2, 3]));
        let mut buf = [0u8; 2];
        // One read is one datagram, truncated to the buffer.
        assert_eq!(d.recv(id, &mut buf), Some(2));
        assert_eq!(d.recv(id, &mut buf), None);
    }

    #[test]
    fn forgotten_remote_starts_again_and_queues_are_bounded() {
        let mut d = UdpDemux::new(true);
        d.deliver(1u16, vec![0]);
        let (id, _) = d.accept().unwrap();
        d.forget(id);
        assert!(!d.pending());
        d.deliver(1, vec![0]);
        let (again, _) = d.accept().unwrap();
        assert_ne!(again, id);
        for _ in 1..UDP_QUEUE {
            assert!(d.deliver(1, vec![0]));
        }
        assert!(!d.deliver(1, vec![0]));
    }
}
