//! Tickets (`docs/lethe.md`, "The ticket"): a client's identity, rights,
//! prefix and session secret, sealed under a key only the servers hold.
//! Any server with the key can open it, so replicas share nothing else.
//!
//! ```text
//! ticket = keyid u32 || nonce[24] || XChaCha20-Poly1305(key, aad, plain)
//! aad    = "lethe1 ticket" 0x00 || keyid
//! plain  = master[32] issued u64 expires u64 rights u8 uname s[2] prefix s[2]
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use sha2::{Digest, Sha256};

use crate::error::{TICKET, TICKET_KEY};
use crate::wire::{Reader, Writer};
use crate::{NONCE, TAG};

const AAD: &[u8] = b"lethe1 ticket\0";
const KEYID: &[u8] = b"lethe1 ticket key\0";

/// The longest ticket a server makes or takes.
pub const MAX_TICKET: usize = 4 + NONCE + TAG + 32 + 17 + 2 + 255 + 2 + crate::path::MAX_PATH;

/// What a ticket holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub master: [u8; 32],
    /// Server time it was issued, seconds since 1970.
    pub issued: u64,
    /// Server time it stops working.
    pub expires: u64,
    pub rights: u8,
    pub uname: String,
    pub prefix: String,
}

impl Ticket {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        let mut w = Writer { buf: &mut v };
        w.bytes(&self.master);
        w.u64(self.issued);
        w.u64(self.expires);
        w.u8(self.rights);
        w.data16(self.uname.as_bytes());
        w.data16(self.prefix.as_bytes());
        v
    }

    fn decode(buf: &[u8]) -> Result<Ticket, &'static str> {
        let mut r = Reader::new(buf);
        let t = Ticket {
            master: r.array()?,
            issued: r.u64()?,
            expires: r.u64()?,
            rights: r.u8()?,
            uname: String::from(r.str16()?),
            prefix: String::from(r.str16()?),
        };
        r.finish()?;
        Ok(t)
    }
}

/// A ticket key: 32 secret bytes, and the id tickets name it by.
#[derive(Clone)]
pub struct TicketKey {
    key: [u8; 32],
    pub id: u32,
}

impl TicketKey {
    pub fn new(key: [u8; 32]) -> TicketKey {
        let mut h = Sha256::new();
        h.update(KEYID);
        h.update(key);
        let d = h.finalize();
        TicketKey {
            key,
            id: u32::from_le_bytes([d[0], d[1], d[2], d[3]]),
        }
    }

    /// `t` sealed under this key with `nonce`, which must be random.
    pub fn seal(&self, t: &Ticket, nonce: &[u8; NONCE]) -> Vec<u8> {
        let id = self.id.to_le_bytes();
        let mut aad = Vec::from(AAD);
        aad.extend_from_slice(&id);
        let sealed = crate::crypto::seal(&self.key, nonce, &aad, &t.encode());
        let mut v = Vec::with_capacity(4 + NONCE + sealed.len());
        v.extend_from_slice(&id);
        v.extend_from_slice(nonce);
        v.extend_from_slice(&sealed);
        v
    }

    /// Opens a ticket sealed under this key. Fails with `bad ticket`.
    pub fn open(&self, ticket: &[u8]) -> Result<Ticket, &'static str> {
        if ticket.len() > MAX_TICKET || ticket.len() < 4 + NONCE + TAG {
            return Err(TICKET);
        }
        let (id, rest) = ticket.split_at(4);
        let (nonce, sealed) = rest.split_at(NONCE);
        let nonce: &[u8; NONCE] = nonce.try_into().map_err(|_| TICKET)?;
        let mut aad = Vec::from(AAD);
        aad.extend_from_slice(id);
        let plain = crate::crypto::open(&self.key, nonce, &aad, sealed).map_err(|_| TICKET)?;
        Ticket::decode(&plain).map_err(|_| TICKET)
    }
}

/// The id a ticket names its key by. `None` when it is too short.
pub fn key_id(ticket: &[u8]) -> Option<u32> {
    ticket
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Opens `ticket` with whichever of `keys` it names: `unknown ticket key`
/// when none, `bad ticket` when it does not open.
pub fn open(keys: &[TicketKey], ticket: &[u8]) -> Result<Ticket, &'static str> {
    let id = key_id(ticket).ok_or(TICKET)?;
    keys.iter()
        .find(|k| k.id == id)
        .ok_or(TICKET_KEY)?
        .open(ticket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        Ticket {
            master: [7; 32],
            issued: 100,
            expires: 200,
            rights: crate::rights::READ,
            uname: String::from("glenda"),
            prefix: String::from("/lib"),
        }
    }

    #[test]
    fn seal_and_open() {
        let k = TicketKey::new([1; 32]);
        let t = ticket();
        let s = k.seal(&t, &[2; NONCE]);
        assert_eq!(key_id(&s), Some(k.id));
        assert_eq!(k.open(&s).unwrap(), t);
        assert_eq!(open(&[TicketKey::new([3; 32]), k.clone()], &s).unwrap(), t);
        // Another server's key is told apart from a changed ticket.
        assert_eq!(open(&[TicketKey::new([3; 32])], &s), Err(TICKET_KEY));
        for i in 4..s.len() {
            let mut bad = s.clone();
            bad[i] ^= 0x10;
            assert_eq!(
                open(core::slice::from_ref(&k), &bad),
                Err(TICKET),
                "byte {i}"
            );
        }
        let mut bad = s.clone();
        bad[0] ^= 1;
        assert_eq!(open(core::slice::from_ref(&k), &bad), Err(TICKET_KEY));
        assert_eq!(open(core::slice::from_ref(&k), &s[..20]), Err(TICKET));
        assert_eq!(open(&[k], &[]), Err(TICKET));
    }

    /// A fixed vector: the key id and the ticket's bytes for a fixed key
    /// and nonce.
    #[test]
    fn vector() {
        let k = TicketKey::new([0x42; 32]);
        let s = k.seal(&ticket(), &[0; NONCE]);
        assert_eq!(s.len(), 4 + NONCE + 32 + 17 + 8 + 6 + TAG);
        assert_eq!(&s[..4], &k.id.to_le_bytes());
        assert_eq!(&s[4..4 + NONCE], &[0; NONCE]);
        // Pinned, so the format cannot change by accident.
        assert_eq!(k.id, KEY_ID_42);
        assert_eq!(purg_auth::hex::encode(&s[s.len() - TAG..]), TAG_42);
    }

    const KEY_ID_42: u32 = 2_936_791_565;
    const TAG_42: &str = "f1284164e6967271a8b5cff9ab7c3c95";
}
