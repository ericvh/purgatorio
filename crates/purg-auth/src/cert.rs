//! `pgcert1` certificates: one line of text,
//!
//! ```text
//! pgcert1 <name> <key> <expires> <signer>:<signature> [<signer>:<signature> ...]
//! ```
//!
//! Each signature is a signer's Ed25519 signature over [`Cert::body`].
//! Parsing is strict, so a certificate has exactly one encoding.
//!
//! [`Cert2`] is `pgcert2`, the same for a hybrid Ed25519 + ML-DSA-65 key
//! ([`crate::sig`]): its key and every signer's are hybrid keys, written
//! `mldsa65:<hex>`, and each signature is a hybrid one. A certificate has
//! one kind of key throughout, so a hybrid key is never vouched for by an
//! Ed25519 signature alone. [`AnyCert`] is either.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::sig::{HYBRID_PUBLIC, HYBRID_SIGNATURE, HybridPublic, HybridSecret, Id, Public, Secret};
use crate::{PUBLIC, SEED, SIGNATURE, hex};

/// The first word of a version 1 certificate.
pub const MAGIC: &str = "pgcert1";
/// At most this many signatures (the issuer and its countersigners).
pub const MAX_SIGS: usize = 8;
/// A name is at most this many bytes.
pub const MAX_NAME: usize = 255;

pub const EBAD: &str = "bad certificate";
pub const EBADNAME: &str = "bad user name";
pub const EDUPSIG: &str = "already signed by that key";
pub const ETOOMANY: &str = "too many signatures";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert {
    pub name: String,
    pub key: [u8; PUBLIC],
    /// Seconds since 1970 UTC; 0 means it does not expire.
    pub expires: u64,
    /// `(signer public key, signature)` pairs.
    pub sigs: Vec<([u8; PUBLIC], [u8; SIGNATURE])>,
}

/// Whether `name` can be a certificate's (and a `usr/<name>`) user name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == ':')
}

impl Cert {
    /// An unsigned certificate.
    pub fn new(name: &str, key: [u8; PUBLIC], expires: u64) -> Result<Cert, &'static str> {
        if !valid_name(name) {
            return Err(EBADNAME);
        }
        Ok(Cert {
            name: String::from(name),
            key,
            expires,
            sigs: Vec::new(),
        })
    }

    /// The bytes each signer signs.
    pub fn body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(MAGIC.len() + 2 + self.name.len() + PUBLIC + 8);
        b.extend_from_slice(MAGIC.as_bytes());
        b.push(0);
        // `valid_name` keeps the length within a byte.
        b.push(self.name.len() as u8);
        b.extend_from_slice(self.name.as_bytes());
        b.extend_from_slice(&self.key);
        b.extend_from_slice(&self.expires.to_le_bytes());
        b
    }

    /// Adds `seed`'s signature.
    pub fn sign(&mut self, seed: &[u8; SEED]) -> Result<(), &'static str> {
        let signer = crate::public_key(seed);
        if self.sigs.iter().any(|(s, _)| *s == signer) {
            return Err(EDUPSIG);
        }
        if self.sigs.len() >= MAX_SIGS {
            return Err(ETOOMANY);
        }
        let sig = crate::sign(seed, &self.body());
        self.sigs.push((signer, sig));
        Ok(())
    }

    /// Whether `signer` has a signature here that verifies.
    pub fn signed_by(&self, signer: &[u8; PUBLIC]) -> bool {
        let body = self.body();
        self.sigs
            .iter()
            .any(|(s, sig)| s == signer && crate::verify(s, &body, sig))
    }

    /// Whether the certificate has expired at `now` (seconds).
    pub fn expired(&self, now: u64) -> bool {
        self.expires != 0 && now >= self.expires
    }

    /// The certificate as one line, without a newline.
    pub fn encode(&self) -> String {
        let mut s = String::new();
        let _ = write!(
            s,
            "{MAGIC} {} {} {}",
            self.name,
            hex::encode(&self.key),
            self.expires
        );
        for (signer, sig) in &self.sigs {
            let _ = write!(s, " {}:{}", hex::encode(signer), hex::encode(sig));
        }
        s
    }

    /// Parses one certificate line. A single trailing newline is allowed.
    pub fn parse(text: &[u8]) -> Result<Cert, &'static str> {
        let text = text.strip_suffix(b"\n").unwrap_or(text);
        let text = core::str::from_utf8(text).map_err(|_| EBAD)?;
        let mut words = text.split(' ');
        if words.next() != Some(MAGIC) {
            return Err(EBAD);
        }
        let name = words.next().ok_or(EBAD)?;
        let key = hex::array::<PUBLIC>(words.next().ok_or(EBAD)?.as_bytes()).ok_or(EBAD)?;
        let expires = parse_u64(words.next().ok_or(EBAD)?).ok_or(EBAD)?;
        let mut cert = Cert::new(name, key, expires).map_err(|_| EBAD)?;
        for w in words {
            let (signer, sig) = w.split_once(':').ok_or(EBAD)?;
            let signer = hex::array::<PUBLIC>(signer.as_bytes()).ok_or(EBAD)?;
            let sig = hex::array::<SIGNATURE>(sig.as_bytes()).ok_or(EBAD)?;
            if cert.sigs.iter().any(|(s, _)| *s == signer) || cert.sigs.len() >= MAX_SIGS {
                return Err(EBAD);
            }
            cert.sigs.push((signer, sig));
        }
        if cert.sigs.is_empty() || cert.encode() != text {
            return Err(EBAD);
        }
        Ok(cert)
    }
}

/// The first word of a `pgcert2` certificate.
pub const MAGIC2: &str = "pgcert2";
/// At most this many signatures on a `pgcert2`: each is about 10.7 KB of
/// text, and a proof carrying the certificate must fit in one message.
pub const MAX_SIGS2: usize = 4;

/// A `pgcert2` certificate: a signer's hybrid signature over a user name,
/// the user's hybrid public key and an expiry.
///
/// ```text
/// pgcert2 <name> mldsa65:<key> <expires> mldsa65:<signer>:<signature> ...
/// body = "pgcert2" 0x00 || len(name) u8 || name || key[1984] || expires u64 LE
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert2 {
    pub name: String,
    pub key: HybridPublic,
    /// Seconds since 1970 UTC; 0 means it does not expire.
    pub expires: u64,
    /// `(signer's public key, hybrid signature)` pairs.
    pub sigs: Vec<(HybridPublic, Vec<u8>)>,
}

impl Cert2 {
    /// An unsigned certificate.
    pub fn new(name: &str, key: HybridPublic, expires: u64) -> Result<Cert2, &'static str> {
        if !valid_name(name) {
            return Err(EBADNAME);
        }
        Ok(Cert2 {
            name: String::from(name),
            key,
            expires,
            sigs: Vec::new(),
        })
    }

    /// The bytes each signer signs.
    pub fn body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(MAGIC2.len() + 2 + self.name.len() + HYBRID_PUBLIC + 8);
        b.extend_from_slice(MAGIC2.as_bytes());
        b.push(0);
        // `valid_name` keeps the length within a byte.
        b.push(self.name.len() as u8);
        b.extend_from_slice(self.name.as_bytes());
        b.extend_from_slice(&self.key.0[..]);
        b.extend_from_slice(&self.expires.to_le_bytes());
        b
    }

    /// Adds `secret`'s signature.
    pub fn sign(&mut self, secret: &HybridSecret) -> Result<(), &'static str> {
        let signer = secret.public();
        if self.sigs.iter().any(|(s, _)| *s == signer) {
            return Err(EDUPSIG);
        }
        if self.sigs.len() >= MAX_SIGS2 {
            return Err(ETOOMANY);
        }
        let sig = secret.sign(&self.body());
        self.sigs.push((signer, sig));
        Ok(())
    }

    /// Whether the signer whose id is `signer` has a signature here that
    /// verifies.
    pub fn signed_by(&self, signer: &[u8; 32]) -> bool {
        let body = self.body();
        self.sigs
            .iter()
            .any(|(s, sig)| s.id() == *signer && s.verify(&body, sig))
    }

    /// Whether the certificate has expired at `now` (seconds).
    pub fn expired(&self, now: u64) -> bool {
        self.expires != 0 && now >= self.expires
    }

    /// The certificate as one line, without a newline.
    pub fn encode(&self) -> String {
        let mut s = String::new();
        let _ = write!(
            s,
            "{MAGIC2} {} {} {}",
            self.name,
            self.key.encode(),
            self.expires
        );
        for (signer, sig) in &self.sigs {
            let _ = write!(s, " {}:{}", signer.encode(), hex::encode(sig));
        }
        s
    }

    /// Parses one certificate line. A single trailing newline is allowed.
    pub fn parse(text: &[u8]) -> Result<Cert2, &'static str> {
        let text = text.strip_suffix(b"\n").unwrap_or(text);
        let text = core::str::from_utf8(text).map_err(|_| EBAD)?;
        let mut words = text.split(' ');
        if words.next() != Some(MAGIC2) {
            return Err(EBAD);
        }
        let name = words.next().ok_or(EBAD)?;
        let key = HybridPublic::parse(words.next().ok_or(EBAD)?.as_bytes()).ok_or(EBAD)?;
        let expires = parse_u64(words.next().ok_or(EBAD)?).ok_or(EBAD)?;
        let mut cert = Cert2::new(name, key, expires).map_err(|_| EBAD)?;
        for w in words {
            // `mldsa65:<signer>:<signature>`
            let (signer, sig) = w.rsplit_once(':').ok_or(EBAD)?;
            let signer = HybridPublic::parse(signer.as_bytes()).ok_or(EBAD)?;
            if sig.len() != HYBRID_SIGNATURE * 2 {
                return Err(EBAD);
            }
            let sig = hex::decode(sig.as_bytes()).ok_or(EBAD)?;
            if cert.sigs.iter().any(|(s, _)| *s == signer) || cert.sigs.len() >= MAX_SIGS2 {
                return Err(EBAD);
            }
            cert.sigs.push((signer, sig));
        }
        if cert.sigs.is_empty() || cert.encode() != text {
            return Err(EBAD);
        }
        Ok(cert)
    }
}

/// A certificate of either kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyCert {
    V1(Cert),
    V2(Cert2),
}

impl AnyCert {
    /// Parses a `pgcert1` or a `pgcert2` line.
    pub fn parse(text: &[u8]) -> Result<AnyCert, &'static str> {
        if text.starts_with(MAGIC2.as_bytes()) {
            Cert2::parse(text).map(AnyCert::V2)
        } else {
            Cert::parse(text).map(AnyCert::V1)
        }
    }

    pub fn encode(&self) -> String {
        match self {
            AnyCert::V1(c) => c.encode(),
            AnyCert::V2(c) => c.encode(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            AnyCert::V1(c) => &c.name,
            AnyCert::V2(c) => &c.name,
        }
    }

    pub fn expires(&self) -> u64 {
        match self {
            AnyCert::V1(c) => c.expires,
            AnyCert::V2(c) => c.expires,
        }
    }

    pub fn expired(&self, now: u64) -> bool {
        match self {
            AnyCert::V1(c) => c.expired(now),
            AnyCert::V2(c) => c.expired(now),
        }
    }

    pub fn is_hybrid(&self) -> bool {
        matches!(self, AnyCert::V2(_))
    }

    /// The certified key.
    pub fn key(&self) -> Public {
        match self {
            AnyCert::V1(c) => Public::Ed25519(c.key),
            AnyCert::V2(c) => Public::Hybrid(c.key.clone()),
        }
    }

    /// The ids of its signers, in order.
    pub fn signers(&self) -> Vec<Id> {
        match self {
            AnyCert::V1(c) => c.sigs.iter().map(|(s, _)| Id::Ed25519(*s)).collect(),
            AnyCert::V2(c) => c.sigs.iter().map(|(s, _)| Id::Hybrid(s.id())).collect(),
        }
    }

    /// Whether the signer `id` has a signature here that verifies. An id
    /// of the other kind never does.
    pub fn signed_by(&self, id: &Id) -> bool {
        match (self, id) {
            (AnyCert::V1(c), Id::Ed25519(k)) => c.signed_by(k),
            (AnyCert::V2(c), Id::Hybrid(i)) => c.signed_by(i),
            _ => false,
        }
    }

    /// Adds a signature by `secret`, which must be of the certificate's
    /// kind.
    pub fn sign(&mut self, secret: &Secret) -> Result<(), &'static str> {
        match (self, secret) {
            (AnyCert::V1(c), Secret::Ed25519(s)) => c.sign(s),
            (AnyCert::V2(c), Secret::Hybrid(h)) => c.sign(h),
            _ => Err(EKIND),
        }
    }
}

/// A key of one kind signing a certificate of the other.
pub const EKIND: &str = "signer and key are of different kinds";

/// A decimal with no sign, no leading zeros (except `0`) and no overflow.
pub fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed() -> (Cert, [u8; SEED]) {
        let signer = [9u8; SEED];
        let user = crate::public_key(&[1u8; SEED]);
        let mut c = Cert::new("alice", user, 1_800_000_000).unwrap();
        c.sign(&signer).unwrap();
        (c, signer)
    }

    #[test]
    fn round_trip_and_verify() {
        let (c, signer) = signed();
        let line = c.encode();
        assert!(line.starts_with("pgcert1 alice "));
        let back = Cert::parse(line.as_bytes()).unwrap();
        assert_eq!(back, c);
        let mut nl = line.clone();
        nl.push('\n');
        assert_eq!(Cert::parse(nl.as_bytes()).unwrap(), c);
        assert!(back.signed_by(&crate::public_key(&signer)));
        assert!(!back.signed_by(&crate::public_key(&[8u8; SEED])));
    }

    #[test]
    fn a_changed_field_breaks_the_signature() {
        let (mut c, signer) = signed();
        c.expires += 1;
        assert!(!c.signed_by(&crate::public_key(&signer)));
        let (mut c, signer) = signed();
        c.name = String::from("bob");
        assert!(!c.signed_by(&crate::public_key(&signer)));
    }

    #[test]
    fn countersignatures() {
        let (mut c, signer) = signed();
        assert_eq!(c.sign(&signer), Err(EDUPSIG));
        c.sign(&[10u8; SEED]).unwrap();
        let back = Cert::parse(c.encode().as_bytes()).unwrap();
        assert!(back.signed_by(&crate::public_key(&signer)));
        assert!(back.signed_by(&crate::public_key(&[10u8; SEED])));
        for i in 0..6 {
            c.sign(&[20 + i; SEED]).unwrap();
        }
        assert_eq!(c.sign(&[99u8; SEED]), Err(ETOOMANY));
    }

    #[test]
    fn strict_parsing() {
        let (c, _) = signed();
        let line = c.encode();
        for bad in [
            String::new(),
            line.replace("pgcert1", "pgcert2"),
            line.replace(' ', "  "),
            line.to_uppercase(),
            alloc::format!("{line} "),
            line.replace(" 1800000000 ", " 01800000000 "),
            line.replace(" 1800000000 ", " -1 "),
            line.replace("alice", "a/b"),
            line.replace("alice", ".."),
            line.split(' ').take(4).collect::<Vec<_>>().join(" "),
        ] {
            assert_eq!(Cert::parse(bad.as_bytes()), Err(EBAD), "{bad}");
        }
        let sig = line.rsplit(' ').next().unwrap();
        let dup = alloc::format!("{line} {sig}");
        assert_eq!(Cert::parse(dup.as_bytes()), Err(EBAD));
    }

    #[test]
    fn names() {
        assert!(valid_name("alice"));
        assert!(valid_name("ünïcode"));
        for bad in ["", ".", "..", "a b", "a/b", "a:b", "a\tb", "a\u{7f}"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(!valid_name(&"x".repeat(256)));
        assert!(valid_name(&"x".repeat(255)));
    }

    fn hybrid(n: u8) -> HybridSecret {
        HybridSecret([n; crate::sig::HYBRID_SEED])
    }

    #[test]
    fn pgcert2_round_trip_and_verify() {
        let signer = hybrid(9);
        let mut c = Cert2::new("alice", hybrid(1).public(), 1_800_000_000).unwrap();
        c.sign(&signer).unwrap();
        let line = c.encode();
        assert!(line.starts_with("pgcert2 alice mldsa65:"));
        let back = Cert2::parse(line.as_bytes()).unwrap();
        assert_eq!(back, c);
        let any = AnyCert::parse(line.as_bytes()).unwrap();
        assert!(any.is_hybrid());
        let id = signer.public().id();
        assert!(back.signed_by(&id));
        assert!(any.signed_by(&Id::Hybrid(id)));
        // The same 32 bytes as an Ed25519 signer do not count.
        assert!(!any.signed_by(&Id::Ed25519(id)));
        assert!(!back.signed_by(&hybrid(8).public().id()));
        // A changed field breaks the signature.
        let mut changed = back.clone();
        changed.expires += 1;
        assert!(!changed.signed_by(&id));
        // A pgcert1 parser refuses it, and the other way round.
        assert_eq!(Cert::parse(line.as_bytes()), Err(EBAD));
        let (v1, _) = signed();
        assert_eq!(Cert2::parse(v1.encode().as_bytes()), Err(EBAD));
        assert!(!AnyCert::parse(v1.encode().as_bytes()).unwrap().is_hybrid());
        // Strict parsing.
        for bad in [
            alloc::format!("{line} "),
            line.replace(' ', "  "),
            line.replacen("mldsa65:", "", 1),
            line.replace(" 1800000000 ", " 01800000000 "),
        ] {
            assert_eq!(Cert2::parse(bad.as_bytes()), Err(EBAD));
        }
        // Signer of the wrong kind.
        let mut a = AnyCert::V2(back);
        assert_eq!(a.sign(&Secret::Ed25519([3; SEED])), Err(EKIND));
        a.sign(&Secret::Hybrid(hybrid(10))).unwrap();
        assert_eq!(a.signers().len(), 2);
        let back = AnyCert::parse(a.encode().as_bytes()).unwrap();
        assert!(back.signed_by(&Id::Hybrid(hybrid(10).public().id())));
    }

    #[test]
    fn expiry() {
        let (mut c, _) = signed();
        assert!(!c.expired(1_799_999_999));
        assert!(c.expired(1_800_000_000));
        c.expires = 0;
        assert!(!c.expired(u64::MAX));
    }
}
