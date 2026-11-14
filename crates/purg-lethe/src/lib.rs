//! Lethe, a stateless protocol for reading static file trees
//! (`docs/lethe.md`).
//!
//! - [`msg`]: frames and the requests and replies inside them.
//! - [`path`]: the path rules.
//! - [`aead`]: XChaCha20-Poly1305, from ring on the emu.
//! - [`crypto`]: the key schedule and the sealing of frames.
//! - [`ticket`]: tickets, sealed under a key only the servers hold.
//! - [`server`]: a server engine over a [`server::Tree`]: one frame in,
//!   one frame out, nothing kept.
//! - [`client`]: the client's side: the ticket exchange, sealed requests
//!   and their replies, and clear requests for a server that allows them.
//!
//! The crate is `no_std` + `alloc`: the kernel's `#λ` device and the host
//! server `lethed` share it.
#![no_std]

extern crate alloc;

pub mod aead;
pub mod client;
pub mod crypto;
pub mod msg;
pub mod path;
pub mod server;
pub mod ticket;
mod wire;

/// The protocol version, the fifth byte of every frame.
pub const VERSION: u8 = 1;
/// A frame's header: `size[4] version[1] type[1] id[4]`.
pub const HEADER: usize = 10;
/// The largest frame.
pub const MAX_FRAME: usize = 65536;
/// The largest `count` of a read or a directory read.
pub const MAX_COUNT: u32 = 32768;
/// The most names one `Twalk` carries.
pub const MAX_WALK: usize = 64;
/// An XChaCha20-Poly1305 nonce.
pub const NONCE: usize = 24;
/// An XChaCha20-Poly1305 tag.
pub const TAG: usize = 16;
/// An X25519 public key.
pub const EPH: usize = 32;
/// An ML-KEM-768 encapsulation key.
pub const KEM_PUBLIC: usize = purg_abi::auth::KEM_PUBLIC;
/// An ML-KEM-768 ciphertext.
pub const KEM_CT: usize = purg_abi::auth::KEM_CIPHERTEXT;

/// The user a clear request runs as: Inferno's unauthenticated identity.
pub const NONE: &str = "none";

/// The rights a ticket can carry.
pub mod rights {
    pub const READ: u8 = 1;
    pub const WRITE: u8 = 2;
    pub const STREAM: u8 = 4;
    /// Reserved: asks for the server's proof in `Rauth` (later).
    pub const SERVER_PROOF: u8 = 0x80;
    /// In a ticket only: it came from the hybrid X25519 + ML-KEM-768
    /// exchange (`Tauth2`). A server that requires that exchange refuses
    /// tickets without it. Never granted on request.
    pub const PQ: u8 = 0x40;
    /// The bits a server may grant.
    pub const GRANTABLE: u8 = READ | WRITE | STREAM;
}

/// Outer frame types.
pub mod kind {
    pub const TAUTH: u8 = 1;
    pub const RAUTH: u8 = 2;
    pub const TREQ: u8 = 3;
    pub const RREQ: u8 = 4;
    pub const RERROR: u8 = 5;
    /// A request with no ticket, not sealed: the clear mode
    /// (`docs/lethe.md`, Section 4).
    pub const TCLEAR: u8 = 9;
    /// A reply, not sealed.
    pub const RCLEAR: u8 = 10;
    /// `Tauth` with the client's ephemeral ML-KEM-768 key: the hybrid
    /// post-quantum exchange (`docs/lethe.md`, "The post-quantum
    /// exchange").
    pub const TAUTH2: u8 = 11;
    /// `Rauth` with the server's ML-KEM-768 ciphertext.
    pub const RAUTH2: u8 = 12;
    /// Reserved for streams and later frames: 6 to 15, less the clear
    /// and the post-quantum frames above.
    pub const RESERVED: core::ops::RangeInclusive<u8> = 6..=15;
}

/// Inner request and reply codes.
pub mod op {
    pub const TSTAT: u8 = 0x10;
    pub const RSTAT: u8 = 0x11;
    pub const TREAD: u8 = 0x12;
    pub const RREAD: u8 = 0x13;
    pub const TREADDIR: u8 = 0x14;
    pub const RREADDIR: u8 = 0x15;
    /// Reserved: the SHA-256 of a file's content.
    pub const TDIGEST: u8 = 0x16;
    pub const RDIGEST: u8 = 0x17;
    /// A walk of several names from a directory in one request. Added in
    /// version 1 at a code older servers answer `not implemented`.
    pub const TWALK: u8 = 0x18;
    pub const RWALK: u8 = 0x19;
    /// Reserved for writes.
    pub const WRITES: core::ops::RangeInclusive<u8> = 0x20..=0x2f;
    /// Reserved for streams.
    pub const STREAMS: core::ops::RangeInclusive<u8> = 0x40..=0x4f;
    pub const RERROR: u8 = 0x7f;
}

/// Errors Lethe adds to Inferno's (`docs/lethe.md`, Section 7).
pub mod error {
    pub const VERSION: &str = "unknown lethe version";
    pub const MESSAGE: &str = "bad lethe message";
    pub const TICKET: &str = "bad ticket";
    pub const TICKET_KEY: &str = "unknown ticket key";
    pub const TICKET_EXPIRED: &str = "ticket expired";
    pub const REQUEST_EXPIRED: &str = "request expired";
    pub const MAC: &str = purg_auth::record::BAD_MAC;
    /// A clear request (`Tclear`) to a server that was not started with
    /// `-A`: Styx's answer to an attach without a proof.
    pub const AUTH_REQUIRED: &str = purg_abi::auth::REQUIRED;
    /// A `Tauth` to a server started with `-A` that trusts no key for the
    /// user: Styx's answer to a `Tauth` on an export with `-A`.
    pub const AUTH_NOT_REQUIRED: &str = purg_abi::auth::NOT_REQUIRED;

    /// Whether a client that gets `e` in the clear should authenticate
    /// again and resend.
    pub fn renew(e: &str) -> bool {
        [TICKET, TICKET_KEY, TICKET_EXPIRED, REQUEST_EXPIRED].contains(&e)
    }
}
