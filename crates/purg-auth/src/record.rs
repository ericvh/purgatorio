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
//!
//! Neither end allocates per record on its hot path: [`Sealer::seal_buf`]
//! builds each record in a buffer the sealer keeps, and
//! [`Opener::open_body`] opens a record read without its header in place
//! and leaves the plaintext where the ciphertext was.

use alloc::vec::Vec;

use cipher::Cipher;

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
/// [`LIMIT`] records have gone one way under one key.
pub const EXHAUSTED: &str = "record sequence exhausted";

/// The most records one direction carries. The nonce is the 64-bit
/// sequence number, so it could count to 2^64 without repeating; the
/// limit stops 2^16 times short of that, with a clear error, so no
/// arithmetic near the end of the counter is ever exercised. At a
/// million records a second it takes nine years to reach: a conversation
/// that does is ended, not rekeyed (`docs/auth.md`, "Record layer").
pub const LIMIT: u64 = 1 << 48;

/// The record layer's error for `s`, when it is one: lets a caller that
/// only kept the text have the `'static` string back.
pub fn error(s: &str) -> Option<&'static str> {
    [BAD_MAC, TOO_LONG, TOO_SHORT, EXHAUSTED]
        .into_iter()
        .find(|e| *e == s)
}

fn nonce(seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&seq.to_le_bytes());
    n
}

/// The AEAD itself. The kernel and the guests use the RustCrypto crate,
/// which is `no_std`; the hosted emu turns on the `ring` feature, whose
/// assembly is about three times faster. The records are the same bytes
/// either way.
mod cipher {
    use super::TAG;

    /// ChaCha20-Poly1305 from the RustCrypto crates, at 0.11, whose
    /// ChaCha20 uses NEON on aarch64 (the kernel; `docs/perf.md`).
    #[cfg_attr(feature = "ring", allow(dead_code))]
    pub struct RustCrypto(chacha20poly1305_rec::ChaCha20Poly1305);

    #[cfg_attr(feature = "ring", allow(dead_code))]
    impl RustCrypto {
        pub fn new(key: &[u8; 32]) -> RustCrypto {
            use chacha20poly1305_rec::KeyInit;
            RustCrypto(chacha20poly1305_rec::ChaCha20Poly1305::new(key.into()))
        }

        pub fn seal(&self, nonce: [u8; 12], aad: &[u8], buf: &mut [u8]) -> Option<[u8; TAG]> {
            use chacha20poly1305_rec::AeadInOut;
            self.0
                .encrypt_inout_detached(&nonce.into(), aad, buf.into())
                .ok()
                .map(Into::into)
        }

        /// Opens `buf`, the ciphertext followed by its tag, in place.
        pub fn open(&self, nonce: [u8; 12], aad: &[u8], buf: &mut [u8]) -> Option<()> {
            use chacha20poly1305_rec::AeadInOut;
            let at = buf.len().checked_sub(TAG)?;
            let (body, tag) = buf.split_at_mut(at);
            let tag: [u8; TAG] = (&*tag).try_into().ok()?;
            self.0
                .decrypt_inout_detached(&nonce.into(), aad, body.into(), &tag.into())
                .ok()
        }
    }

    /// ChaCha20-Poly1305 from `ring`.
    #[cfg(feature = "ring")]
    pub struct Ring(ring::aead::LessSafeKey);

    #[cfg(feature = "ring")]
    impl Ring {
        pub fn new(key: &[u8; 32]) -> Ring {
            use ring::aead::{CHACHA20_POLY1305, LessSafeKey, UnboundKey};
            // A 32-byte key is always the right length.
            let key = UnboundKey::new(&CHACHA20_POLY1305, key).unwrap_or_else(|_| unreachable!());
            Ring(LessSafeKey::new(key))
        }

        pub fn seal(&self, nonce: [u8; 12], aad: &[u8], buf: &mut [u8]) -> Option<[u8; TAG]> {
            use ring::aead::{Aad, Nonce};
            let tag = self
                .0
                .seal_in_place_separate_tag(
                    Nonce::assume_unique_for_key(nonce),
                    Aad::from(aad),
                    buf,
                )
                .ok()?;
            tag.as_ref().try_into().ok()
        }

        /// Opens `buf`, the ciphertext followed by its tag, in place.
        pub fn open(&self, nonce: [u8; 12], aad: &[u8], buf: &mut [u8]) -> Option<()> {
            use ring::aead::{Aad, Nonce};
            self.0
                .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), buf)
                .ok()
                .map(drop)
        }
    }

    #[cfg(feature = "ring")]
    pub type Cipher = Ring;
    #[cfg(not(feature = "ring"))]
    pub type Cipher = RustCrypto;
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
    aead: Cipher,
    seq: u64,
    /// The last record [`Sealer::seal_buf`] made; its capacity is reused.
    buf: Vec<u8>,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Sealer {
        Sealer::starting_at(key, 0)
    }

    /// A sealer whose next record is number `seq`: for tests of the
    /// sequence's end.
    pub fn starting_at(key: &[u8; 32], seq: u64) -> Sealer {
        Sealer {
            aead: Cipher::new(key),
            seq,
            buf: Vec::new(),
        }
    }

    /// The next record's sequence number.
    pub fn sequence(&self) -> u64 {
        self.seq
    }

    /// `plain` as the next record.
    pub fn seal(&mut self, plain: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut rec = Vec::new();
        self.seal_into(plain, &mut rec)?;
        Ok(rec)
    }

    /// `plain` as the next record, in a buffer the sealer keeps and reuses
    /// for the next one, so a stream of records allocates nothing once the
    /// buffer has grown to the largest.
    pub fn seal_buf(&mut self, plain: &[u8]) -> Result<&[u8], &'static str> {
        let mut rec = core::mem::take(&mut self.buf);
        let r = self.seal_into(plain, &mut rec);
        self.buf = rec;
        r.map(|()| &self.buf[..])
    }

    /// Replaces what `rec` holds with `plain` sealed as the next record.
    fn seal_into(&mut self, plain: &[u8], rec: &mut Vec<u8>) -> Result<(), &'static str> {
        if self.seq >= LIMIT {
            return Err(EXHAUSTED);
        }
        let n = u32::try_from(plain.len() + OVERHEAD).map_err(|_| TOO_LONG)?;
        let header = n.to_le_bytes();
        rec.clear();
        rec.reserve(n as usize);
        rec.extend_from_slice(&header);
        rec.extend_from_slice(plain);
        let tag = self
            .aead
            .seal(nonce(self.seq), &header, &mut rec[HEADER..])
            .ok_or(TOO_LONG)?;
        rec.extend_from_slice(&tag);
        self.seq += 1;
        Ok(())
    }
}

/// The receiving end of one direction.
pub struct Opener {
    aead: Cipher,
    seq: u64,
    /// A record failed: nothing more opens.
    failed: bool,
}

impl Opener {
    pub fn new(key: &[u8; 32]) -> Opener {
        Opener::starting_at(key, 0)
    }

    /// An opener whose next record is number `seq`: for tests of the
    /// sequence's end.
    pub fn starting_at(key: &[u8; 32], seq: u64) -> Opener {
        Opener {
            aead: Cipher::new(key),
            seq,
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

    /// Opens the next record given as its `header` and the rest of it,
    /// `body` (the ciphertext and the tag), in place: on success `body`
    /// holds the plaintext. A reader that takes the header first (to learn
    /// the size, [`size`]) reads the rest straight into `body`, so the
    /// plaintext needs no move. After a failure every later record fails
    /// too.
    pub fn open_body(
        &mut self,
        header: [u8; HEADER],
        body: &mut Vec<u8>,
    ) -> Result<(), &'static str> {
        if self.failed {
            return Err(BAD_MAC);
        }
        let result = self.open_parts(header, body);
        if result.is_err() {
            self.failed = true;
        }
        result.map(|n| body.truncate(n))
    }

    fn open_in_place(&mut self, rec: &mut Vec<u8>) -> Result<(), &'static str> {
        let header: [u8; HEADER] = rec
            .get(..HEADER)
            .and_then(|h| h.try_into().ok())
            .ok_or(TOO_SHORT)?;
        let n = self.open_parts(header, &mut rec[HEADER..])?;
        rec.truncate(HEADER + n);
        rec.drain(..HEADER);
        Ok(())
    }

    /// Opens `body` under `header` in place; returns the plaintext's length.
    fn open_parts(&mut self, header: [u8; HEADER], body: &mut [u8]) -> Result<usize, &'static str> {
        if size(header, usize::MAX)? != body.len() + HEADER {
            return Err(BAD_MAC);
        }
        if self.seq >= LIMIT {
            return Err(EXHAUSTED);
        }
        self.aead
            .open(nonce(self.seq), &header, body)
            .ok_or(BAD_MAC)?;
        self.seq += 1;
        Ok(body.len() - TAG)
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

    /// `seal_buf` makes the same records as `seal`, and `open_body` opens
    /// them from the header and the body apart.
    #[test]
    fn buffers_and_bodies() {
        let (mut a, mut b) = (Sealer::new(&KEY), Sealer::new(&KEY));
        let mut o = Opener::new(&KEY);
        for i in [0usize, 1, 100, 8216, 3, 0] {
            let msg = vec![i as u8; i];
            let want = a.seal(&msg).unwrap();
            let got = b.seal_buf(&msg).unwrap();
            assert_eq!(got, &want[..], "len {i}");
            let header = want[..HEADER].try_into().unwrap();
            let mut body = want[HEADER..].to_vec();
            o.open_body(header, &mut body).unwrap();
            assert_eq!(body, msg);
        }
        // A changed body fails, and so does everything after it.
        let rec = a.seal(b"x").unwrap();
        let mut body = rec[HEADER..].to_vec();
        body[0] ^= 1;
        let header = rec[..HEADER].try_into().unwrap();
        assert_eq!(o.open_body(header, &mut body), Err(BAD_MAC));
        let rec = a.seal(b"y").unwrap();
        assert_eq!(o.open(rec), Err(BAD_MAC));
        // A header that does not match the body's length.
        let mut o = Opener::new(&KEY);
        let rec = Sealer::new(&KEY).seal(b"abc").unwrap();
        let mut body = rec[HEADER..rec.len() - 1].to_vec();
        assert_eq!(
            o.open_body(rec[..HEADER].try_into().unwrap(), &mut body),
            Err(BAD_MAC)
        );
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

    /// The last records before [`LIMIT`] go and open; the one after fails
    /// cleanly on both ends.
    #[test]
    fn the_sequence_ends_with_an_error() {
        let mut s = Sealer::starting_at(&KEY, LIMIT - 2);
        let mut o = Opener::starting_at(&KEY, LIMIT - 2);
        for m in [&b"a"[..], b"b"] {
            assert_eq!(o.open(s.seal(m).unwrap()).unwrap(), m);
        }
        assert_eq!(s.sequence(), LIMIT);
        assert_eq!(s.seal(b"c"), Err(EXHAUSTED));
        // A record sealed past the limit by a peer that does not stop.
        let rec = Sealer::starting_at(&KEY, LIMIT - 1).seal(b"x").unwrap();
        let mut o = Opener::starting_at(&KEY, LIMIT);
        assert_eq!(o.open(rec), Err(EXHAUSTED));
        assert_eq!(error(EXHAUSTED), Some(EXHAUSTED));
    }

    /// With the `ring` feature both ciphers are built: each opens what the
    /// other sealed, byte for byte.
    #[cfg(feature = "ring")]
    #[test]
    fn ring_and_rustcrypto_agree() {
        use super::cipher::{Ring, RustCrypto};
        let (a, b) = (RustCrypto::new(&KEY), Ring::new(&KEY));
        for len in [0usize, 1, 63, 64, 65, 1000, 8216] {
            let plain: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let (mut x, mut y) = (plain.clone(), plain.clone());
            let n = nonce(len as u64);
            let tx = a.seal(n, b"hdr", &mut x).unwrap();
            let ty = b.seal(n, b"hdr", &mut y).unwrap();
            assert_eq!((&x, tx), (&y, ty), "len {len}");
            x.extend_from_slice(&tx);
            y.extend_from_slice(&ty);
            assert!(b.open(n, b"hdr", &mut x).is_some());
            assert!(a.open(n, b"hdr", &mut y).is_some());
            assert_eq!(&x[..len], &plain[..]);
            assert_eq!(&y[..len], &plain[..]);
            y[0] ^= 1;
            assert!(a.open(n, b"hdr", &mut y).is_none());
        }
    }
}
