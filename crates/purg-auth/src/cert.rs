//! `pgcert1` certificates: one line of text,
//!
//! ```text
//! pgcert1 <name> <key> <expires> <signer>:<signature> [<signer>:<signature> ...]
//! ```
//!
//! Each signature is a signer's Ed25519 signature over [`Cert::body`].
//! Parsing is strict, so a certificate has exactly one encoding.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

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

    #[test]
    fn expiry() {
        let (mut c, _) = signed();
        assert!(!c.expired(1_799_999_999));
        assert!(c.expired(1_800_000_000));
        c.expires = 0;
        assert!(!c.expired(u64::MAX));
    }
}
