//! The connection after the opening exchange: frames, each sealed as one
//! record of `purg_auth::record` once the exchange has agreed keys
//! (`docs/rcmd.md`, "Encryption").
//!
//! The keys come from the version 3 mount handshake's X25519 exchange
//! (`docs/auth.md`): the server's CHAL is its offer, `challenge[32] ||
//! S[32]`, and the client's PROOF a version 3 proof over it, which carries
//! `C` and binds both keys to the client's identity. The PROOF frame and
//! the server's answer to it (OK or ERR) are the last frames in the clear;
//! from the next frame on, each direction is a sequence of records:
//!
//! ```text
//! record = size[4] || sealed(kind u8 || length u32 || data) || tag[16]
//! ```
//!
//! A record that fails to open ends the connection with `bad record mac`.
//! With `-A` there are no keys and frames go as they are.

use std::io::Read;

use purgatorio::auth::fmt::record::{self, HEADER, Opener, Sealer};
use purgatorio::auth::fmt::session::Keys;

use crate::proto::{self, Decoder, EHUNGUP, EPROTO, HDR, MAX};

/// One end's view of the connection: how it frames what it sends and
/// takes apart what it receives.
pub struct Link {
    seal: Option<Sealer>,
    open: Option<Opener>,
    /// Received bytes not yet a whole record.
    buf: Vec<u8>,
    /// Frames in the clear.
    dec: Decoder,
}

impl Link {
    /// A link in the clear.
    pub fn clear() -> Link {
        Link {
            seal: None,
            open: None,
            buf: Vec::new(),
            dec: Decoder::default(),
        }
    }

    /// The client's end of an encrypted link.
    pub fn client(keys: &Keys) -> Link {
        let (seal, open) = keys.client();
        Link::sealed(seal, open)
    }

    /// The server's end of an encrypted link.
    pub fn server(keys: &Keys) -> Link {
        let (seal, open) = keys.server();
        Link::sealed(seal, open)
    }

    fn sealed(seal: Sealer, open: Opener) -> Link {
        Link {
            seal: Some(seal),
            open: Some(open),
            ..Link::clear()
        }
    }

    /// Whether frames are sealed.
    pub fn encrypted(&self) -> bool {
        self.seal.is_some()
    }

    /// A frame as it goes on the wire, ready to write in one write.
    pub fn wire(&mut self, kind: u8, data: &[u8]) -> Result<Vec<u8>, &'static str> {
        let f = proto::frame(kind, data);
        match &mut self.seal {
            Some(s) => s.seal(&f),
            None => Ok(f),
        }
    }

    /// Takes bytes that arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.open.is_some() {
            self.buf.extend_from_slice(bytes);
        } else {
            self.dec.push(bytes);
        }
    }

    /// The next whole frame, `Ok(None)` when more bytes are needed.
    pub fn take(&mut self) -> Result<Option<(u8, Vec<u8>)>, &'static str> {
        let Some(open) = self.open.as_mut() else {
            return self.dec.take();
        };
        let Some(header) = self.buf.get(..HEADER) else {
            return Ok(None);
        };
        let header: [u8; HEADER] = header.try_into().map_err(|_| EPROTO)?;
        let n = record::size(header, HDR + MAX)?;
        if self.buf.len() < n {
            return Ok(None);
        }
        let rec: Vec<u8> = self.buf.drain(..n).collect();
        unframe(&open.open(rec)?).map(Some)
    }

    /// Reads exactly one frame from `r`, and no byte past it. Only for the
    /// opening, before anything has been pushed.
    pub fn read(&mut self, r: &mut impl Read) -> Result<(u8, Vec<u8>), String> {
        let Some(open) = self.open.as_mut() else {
            return proto::read_frame(r);
        };
        let mut header = [0u8; HEADER];
        r.read_exact(&mut header)
            .map_err(|_| String::from(EHUNGUP))?;
        let n = record::size(header, HDR + MAX).map_err(String::from)?;
        let mut rec = vec![0u8; n];
        rec[..HEADER].copy_from_slice(&header);
        r.read_exact(&mut rec[HEADER..])
            .map_err(|_| String::from(EHUNGUP))?;
        let plain = open.open(rec).map_err(String::from)?;
        unframe(&plain).map_err(String::from)
    }
}

/// The one frame a record holds.
fn unframe(p: &[u8]) -> Result<(u8, Vec<u8>), &'static str> {
    if p.len() < HDR {
        return Err(EPROTO);
    }
    let len = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
    if len > MAX || len != p.len() - HDR {
        return Err(EPROTO);
    }
    Ok((p[0], p[HDR..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::kind;

    fn keys() -> Keys {
        Keys {
            c2s: [1; 32],
            s2c: [2; 32],
            transcript: [3; 32],
            bind: [4; 32],
        }
    }

    #[test]
    fn frames_go_sealed_both_ways() {
        let (mut c, mut s) = (Link::client(&keys()), Link::server(&keys()));
        let mut wire = c.wire(kind::IN, b"hello").unwrap();
        wire.extend(c.wire(kind::STYX, &[7u8; 3000]).unwrap());
        assert!(!wire.windows(5).any(|w| w == b"hello"));
        // A byte at a time: a frame appears only when its record is whole.
        let mut got = Vec::new();
        for b in &wire {
            s.push(&[*b]);
            while let Some(f) = s.take().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got[0], (kind::IN, b"hello".to_vec()));
        assert_eq!(got[1], (kind::STYX, vec![7u8; 3000]));
        let back = s.wire(kind::STATUS, b"done").unwrap();
        assert_eq!(
            c.read(&mut &back[..]).unwrap(),
            (kind::STATUS, b"done".to_vec())
        );
    }

    #[test]
    fn a_changed_or_replayed_record_ends_the_link() {
        let (mut c, mut s) = (Link::client(&keys()), Link::server(&keys()));
        let mut one = c.wire(kind::IN, b"one").unwrap();
        one[6] ^= 1;
        s.push(&one);
        assert_eq!(s.take(), Err(record::BAD_MAC));
        let (mut c, mut s) = (Link::client(&keys()), Link::server(&keys()));
        let one = c.wire(kind::IN, b"one").unwrap();
        s.push(&one);
        assert!(s.take().unwrap().is_some());
        s.push(&one);
        assert_eq!(s.take(), Err(record::BAD_MAC));
        // A clear frame is not a record: it never opens as one.
        let mut s = Link::server(&keys());
        s.push(&proto::frame(kind::IN, b""));
        assert_eq!(s.take(), Err(record::TOO_SHORT));
        let mut s = Link::server(&keys());
        let mut forged = vec![0u8; 45];
        forged[0] = 45;
        s.push(&forged);
        assert_eq!(s.take(), Err(record::BAD_MAC));
        // And the keys of the other direction do not open it.
        let mut c = Link::client(&keys());
        let mut c2 = Link::client(&keys());
        c2.push(&c.wire(kind::IN, b"x").unwrap());
        assert_eq!(c2.take(), Err(record::BAD_MAC));
    }
}
