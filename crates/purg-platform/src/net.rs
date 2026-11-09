//! Byte streams the IP device (`#I`) and the command device (`#C`) need.
//! The hosted emu implements them with sockets and processes. The probe
//! implements TCP with its own stack. Both sit behind these traits so
//! `purg-core` stays `no_std`.

use alloc::string::String;
use alloc::sync::Arc;

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
    fn dial_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>>;
    fn announce_udp(&self, host: &str, port: u16) -> Result<Arc<dyn Conn>>;
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
