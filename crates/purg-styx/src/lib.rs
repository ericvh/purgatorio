//! Styx, Inferno's name for 9P2000: message, `Qid` and `Dir` codecs.
//!
//! Decoding is zero-copy over `&[u8]` and uses only little-endian byte
//! accessors, so it is alignment-agnostic on every target.
#![no_std]

extern crate alloc;

mod dir;
mod fcall;
pub mod server;
mod wire;

pub use dir::{
    DMAPPEND, DMAUTH, DMDIR, DMEXCL, Dir, QTAPPEND, QTAUTH, QTDIR, QTEXCL, QTFILE, Qid, STATFIXLEN,
    dirs,
};
pub use fcall::{Fcall, MAXWELEM, Msg, NOFID, NOTAG, VERSION, kind};
pub use server::{IOHDRSZ, MAX_MSIZE, StyxFs, dispatch};

/// Codec failures. None of these can be caused by valid 9P2000 traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The buffer ended before the message or field did.
    Short,
    /// The message size exceeds the negotiated `msize`.
    TooBig,
    /// Unknown or illegal message type byte (including `Terror`).
    BadType(u8),
    /// A string field is not valid UTF-8.
    BadString,
    /// A walk carried more than [`MAXWELEM`] elements.
    TooManyWalk,
    /// Bytes left over after the last field of a message or stat entry.
    Trailing,
    /// A value does not fit in its wire field.
    Overflow,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Short => f.write_str("short 9P message"),
            Error::TooBig => f.write_str("9P message larger than msize"),
            Error::BadType(t) => write!(f, "bad 9P message type {t}"),
            Error::BadString => f.write_str("9P string is not UTF-8"),
            Error::TooManyWalk => f.write_str("too many walk elements"),
            Error::Trailing => f.write_str("trailing bytes in 9P message"),
            Error::Overflow => f.write_str("value too large for 9P field"),
        }
    }
}

/// Reads the `size[4]` prefix of a message, if enough bytes are present.
/// Useful for framing a byte stream.
pub fn message_size(buf: &[u8]) -> Option<usize> {
    let b: [u8; 4] = buf.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(b) as usize)
}
