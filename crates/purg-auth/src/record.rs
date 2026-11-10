//! The record layer that protects a Styx conversation after a version 3
//! mount handshake (`docs/auth.md`, "Record layer").
//!
//! Each direction has its own ChaCha20-Poly1305 key and counts its records
//! from 0. A record is
//!
//! ```text
//! size[4] || ciphertext || tag[16]
//! ```
//!
//! where `size` (u32 LE) is the length of the whole record, these four
//! bytes included, as a Styx message's `size` is. The nonce is the record's
//! sequence number (u64 LE) followed by four zero bytes, and the header is
//! the associated data. The sequence number is never sent, so a record
//! that is replayed, dropped, reordered or changed fails to open, and so
//! does every record after it: the conversation is over. A stream that
//! ends inside a record is a hangup.
//!
//! The mount client and `export` send one Styx message per record, so the
//! largest record is the negotiated msize plus [`OVERHEAD`].

use alloc::vec::Vec;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};

/// The length of a record's header.
pub const HEADER: usize = 4;
/// The length of a record's authentication tag.
pub const TAG: usize = 16;
/// What a record adds to its plaintext.
pub const OVERHEAD: usize = HEADER + TAG;

/// A record that does not open: changed, replayed, reordered or under
/// another key.
pub const BAD_MAC: &str = "bad record mac";
/// A record longer than the receiver takes.
pub const TOO_LONG: &str = "record too long";
/// A record too short to hold a tag.
pub const TOO_SHORT: &str = "record too short";
/// 2^64 records have been sent under one key.
pub const EXHAUSTED: &str = "record sequence exhausted";

/// The record layer's error for `s`, when it is one: lets a caller that
/// only kept the text have the `'static` string back.
pub fn error(s: &str) -> Option<&'static str> {
    [BAD_MAC, TOO_LONG, TOO_SHORT, EXHAUSTED]
        .into_iter()
        .find(|e| *e == s)
}

fn nonce(seq: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&seq.to_le_bytes());
    Nonce::from(n)
}

/// The size of the record whose header is `header`, checked against the
/// largest plaintext the caller takes. The caller reads that many bytes in
/// all, header included, and hands them to [`Opener::open`].
pub fn size(header: [u8; HEADER], max_plain: usize) -> Result<usize, &'static str> {
    let n = u32::from_le_bytes(header) as usize;
    if n < OVERHEAD {
        return Err(TOO_SHORT);
    }
    if n - OVERHEAD > max_plain {
        return Err(TOO_LONG);
    }
    Ok(n)
}

/// The sending end of one direction.
pub struct Sealer {
    aead: ChaCha20Poly1305,
    seq: u64,
    spent: bool,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Sealer {
        Sealer {
            aead: ChaCha20Poly1305::new(Key::from_slice(key)),
            seq: 0,
            spent: false,
        }
    }

    /// The next record's sequence number.
    pub fn sequence(&self) -> u64 {
        self.seq
    }

    /// `plain` as the next record.
    pub fn seal(&mut self, plain: &[u8]) -> Result<Vec<u8>, &'static str> {
        if self.spent {
            return Err(EXHAUSTED);
        }
        let n = u32::try_from(plain.len() + OVERHEAD).map_err(|_| TOO_LONG)?;
        let header = n.to_le_bytes();
        let mut rec = Vec::with_capacity(n as usize);
        rec.extend_from_slice(&header);
        rec.extend_from_slice(plain);
        let tag = self
            .aead
            .encrypt_in_place_detached(&nonce(self.seq), &header, &mut rec[HEADER..])
            .map_err(|_| TOO_LONG)?;
        rec.extend_from_slice(&tag);
        match self.seq.checked_add(1) {
            Some(s) => self.seq = s,
            None => self.spent = true,
        }
        Ok(rec)
    }
}

/// The receiving end of one direction.
pub struct Opener {
    aead: ChaCha20Poly1305,
    seq: u64,
    /// A record failed: nothing more opens.
    failed: bool,
}

impl Opener {
    pub fn new(key: &[u8; 32]) -> Opener {
        Opener {
            aead: ChaCha20Poly1305::new(Key::from_slice(key)),
            seq: 0,
            failed: false,
        }
    }

    /// The next record's sequence number.
    pub fn sequence(&self) -> u64 {
        self.seq
    }

    /// Opens the next record, all of it, header included. Returns the
    /// plaintext. After a failure every later record fails too.
    pub fn open(&mut self, mut rec: Vec<u8>) -> Result<Vec<u8>, &'static str> {
        if self.failed {
            return Err(BAD_MAC);
        }
        let result = self.open_in_place(&mut rec);
        if result.is_err() {
            self.failed = true;
        }
        result.map(|()| rec)
    }

    fn open_in_place(&mut self, rec: &mut Vec<u8>) -> Result<(), &'static str> {
        let header: [u8; HEADER] = rec
            .get(..HEADER)
            .and_then(|h| h.try_into().ok())
            .ok_or(TOO_SHORT)?;
        if size(header, usize::MAX)? != rec.len() {
            return Err(BAD_MAC);
        }
        let body_end = rec.len() - TAG;
        let tag = Tag::clone_from_slice(&rec[body_end..]);
        self.aead
            .decrypt_in_place_detached(&nonce(self.seq), &header, &mut rec[HEADER..body_end], &tag)
            .map_err(|_| BAD_MAC)?;
        self.seq = self.seq.checked_add(1).ok_or(EXHAUSTED)?;
        rec.truncate(body_end);
        rec.drain(..HEADER);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const KEY: [u8; 32] = [0x42; 32];

    fn pair() -> (Sealer, Opener) {
        (Sealer::new(&KEY), Opener::new(&KEY))
    }

    /// The first two records under a fixed key, so the format cannot
    /// change unnoticed. The bytes were computed with a separate
    /// ChaCha20-Poly1305 (RFC 8439, checked against its section 2.8.2
    /// vector), not with this code.
    #[test]
    fn fixed_vectors() {
        let mut s = Sealer::new(&KEY);
        let r0 = s.seal(b"hello").unwrap();
        let r1 = s.seal(b"").unwrap();
        assert_eq!(
            crate::hex::encode(&r0),
            "190000003103884772b192237daae4cd68cd612085212141c3"
        );
        assert_eq!(
            crate::hex::encode(&r1),
            "14000000e9a59791f77032c32a23a5fd045cb042"
        );
        let mut o = Opener::new(&KEY);
        assert_eq!(o.open(r0).unwrap(), b"hello");
        assert_eq!(o.open(r1).unwrap(), b"");
        assert_eq!(o.sequence(), 2);
    }

    #[test]
    fn round_trip() {
        let (mut s, mut o) = pair();
        for i in 0..50usize {
            let msg = vec![i as u8; i * 37];
            let rec = s.seal(&msg).unwrap();
            assert_eq!(rec.len(), msg.len() + OVERHEAD);
            assert_eq!(size(rec[..4].try_into().unwrap(), msg.len()), Ok(rec.len()));
            assert_eq!(o.open(rec).unwrap(), msg);
        }
    }

    /// Any changed bit fails, and so does everything after it.
    #[test]
    fn tamper() {
        let (mut s, _) = pair();
        let rec = s.seal(b"Twalk 1 2 lib").unwrap();
        for i in 0..rec.len() {
            for bit in [0x01u8, 0x80] {
                let mut bad = rec.clone();
                bad[i] ^= bit;
                let mut o = Opener::new(&KEY);
                assert!(o.open(bad).is_err(), "byte {i}");
                assert_eq!(o.open(rec.clone()), Err(BAD_MAC));
            }
        }
    }

    #[test]
    fn replay_and_reorder() {
        let (mut s, mut o) = pair();
        let a = s.seal(b"a").unwrap();
        let b = s.seal(b"b").unwrap();
        assert_eq!(o.open(a.clone()).unwrap(), b"a");
        // Replayed.
        assert_eq!(o.open(a.clone()), Err(BAD_MAC));
        let (mut s, mut o) = pair();
        let a = s.seal(b"a").unwrap();
        let b2 = s.seal(b"b").unwrap();
        // Reordered, or one dropped.
        assert_eq!(o.open(b2), Err(BAD_MAC));
        assert_eq!(o.open(a), Err(BAD_MAC));
        let _ = b;
    }

    /// The other direction's key, or the other direction's records.
    #[test]
    fn wrong_key() {
        let mut s = Sealer::new(&[1; 32]);
        let mut o = Opener::new(&[2; 32]);
        assert_eq!(o.open(s.seal(b"x").unwrap()), Err(BAD_MAC));
    }

    #[test]
    fn truncation_and_sizes() {
        let (mut s, mut o) = pair();
        let rec = s.seal(b"0123456789").unwrap();
        // Cut short, or with the size changed to match the cut.
        assert_eq!(o.open(rec[..rec.len() - 1].to_vec()), Err(BAD_MAC));
        let mut o = Opener::new(&KEY);
        let mut cut = rec[..rec.len() - 1].to_vec();
        cut[..4].copy_from_slice(&((rec.len() - 1) as u32).to_le_bytes());
        assert_eq!(o.open(cut), Err(BAD_MAC));
        let mut o = Opener::new(&KEY);
        assert_eq!(o.open(rec[..3].to_vec()), Err(TOO_SHORT));
        assert_eq!(size(19u32.to_le_bytes(), 100), Err(TOO_SHORT));
        assert_eq!(size(20u32.to_le_bytes(), 0), Ok(20));
        assert_eq!(size(121u32.to_le_bytes(), 100), Err(TOO_LONG));
        assert_eq!(size(u32::MAX.to_le_bytes(), 8216), Err(TOO_LONG));
        assert_eq!(error("bad record mac"), Some(BAD_MAC));
        assert_eq!(error("i/o error"), None);
    }
}
