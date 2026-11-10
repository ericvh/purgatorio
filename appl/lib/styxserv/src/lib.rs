//! Code the Styx tools share: reading whole 9P messages from a byte stream
//! ([`read_msg`]), the one-line text form of a message ([`text`]), a server
//! that passes a directory through with a [`pass::Policy`] in between
//! ([`pass`]), a read-only in-memory tree for archive servers ([`rotree`]),
//! and running a server in the background on a mount point ([`bg`]).

pub mod bg;
pub mod pass;
pub mod rotree;
pub mod text;

use std::io::{self, Read};

pub use purg_styx::{Fcall, IOHDRSZ, MAX_MSIZE, Msg, NOFID, NOTAG, StyxFs, VERSION};

/// The largest message the tools accept from a peer: Inferno's `MAXRPC`
/// rounded up, with room for any `msize` a real server offers.
pub const MAXMSG: usize = 128 * 1024 + IOHDRSZ as usize;

/// Reads one whole 9P message from `r`. `Ok(None)` is a clean end of file
/// before the first byte of a message.
pub fn read_msg(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut hdr = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut hdr[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short 9P message",
            ));
        }
        got += n;
    }
    let size = u32::from_le_bytes(hdr) as usize;
    if !(7..=MAXMSG).contains(&size) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad 9P size"));
    }
    let mut buf = vec![0u8; size];
    buf[..4].copy_from_slice(&hdr);
    r.read_exact(&mut buf[4..])?;
    Ok(Some(buf))
}

/// Decodes a message read by [`read_msg`].
pub fn decode(buf: &[u8]) -> Result<Msg<'_>, String> {
    Msg::decode(buf, MAXMSG as u32).map_err(|e| e.to_string())
}

/// `ename` as the Inferno string a failed `std` call left, for servers that
/// turn file errors into `Rerror`.
pub fn ioerr(e: io::Error) -> String {
    purgatorio::Error::from(e).0
}

/// `path` made absolute against the current directory, so a server child
/// started later finds the same file.
pub fn abs(path: &str) -> String {
    if path.starts_with('/') || path.starts_with('#') {
        return String::from(path);
    }
    match purgatorio::dot() {
        Ok(d) if d.ends_with('/') => format!("{d}{path}"),
        Ok(d) => format!("{d}/{path}"),
        Err(_) => String::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        let m = Msg {
            tag: 3,
            fcall: Fcall::Tclunk { fid: 9 },
        };
        let mut wire = m.encode().unwrap();
        wire.extend(m.encode().unwrap());
        let mut r = &wire[..];
        for _ in 0..2 {
            let b = read_msg(&mut r).unwrap().unwrap();
            assert_eq!(decode(&b).unwrap(), m);
        }
        assert!(read_msg(&mut r).unwrap().is_none());
        let mut short = &wire[..2];
        assert!(read_msg(&mut short).is_err());
    }
}
