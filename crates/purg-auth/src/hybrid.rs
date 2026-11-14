//! `crypt`'s public-key mode: files encrypted to a recipient's hybrid
//! X25519 + ML-KEM-768 key (`docs/auth.md`, "Post-quantum options").
//!
//! A key pair is 96 secret bytes, an X25519 secret and an ML-KEM-768 seed
//! (`d || z`), kept as one text line:
//!
//! ```text
//! pgkemkey1 <96 bytes in hex>
//! pgkempub1 <x25519 public[32] || ml-kem-768 encapsulation key[1184] in hex>
//! ```
//!
//! An encrypted file is a header followed by `pgcrypt1`'s chunks
//! ([`crate::login::stream`]):
//!
//! ```text
//! header = "pgcrypt2" || suite u8 (1) || id[8] || eph[32] || ct[1088] || prefix[7]
//! id     = SHA-256(recipient public key)[..8]
//! eph    = X25519(e, base)                     e: the sender's ephemeral secret
//! ct     = ML-KEM-768.Encaps(recipient ek)     shared secret k
//! x      = X25519(e, recipient x25519 public)  all zeros is refused
//! key    = HKDF-SHA256(salt = SHA-256(header || recipient public key),
//!                      ikm = k || x, info = "pgcrypt2 key", 32)
//! ```
//!
//! The salt binds the whole header (the suite, both ephemeral values and
//! the chunk nonces' prefix) and the recipient's public key, as X-Wing
//! binds the X25519 keys into its combiner. The file is as safe as the
//! stronger of the two key exchanges.

use alloc::string::String;
use alloc::vec::Vec;

use hkdf::Hkdf;
use rand_core::{CryptoRng, RngCore};
use sha2::{Digest, Sha256};

use crate::hex;
use crate::kem;
use crate::session::Ephemeral;

/// The first line word of a secret key file.
pub const SECRET_WORD: &str = "pgkemkey1";
/// The first line word of a public key.
pub const PUBLIC_WORD: &str = "pgkempub1";
/// A secret key: the X25519 secret and the ML-KEM-768 seed.
pub const SECRET: usize = 32 + kem::SEED;
/// A public key: the X25519 public key and the ML-KEM-768 encapsulation key.
pub const PUBLIC: usize = 32 + kem::PUBLIC;
/// What starts an encrypted file.
pub const MAGIC: &[u8; 8] = b"pgcrypt2";
/// The one suite: X25519 with ML-KEM-768.
pub const SUITE: u8 = 1;
/// The key id's length.
pub const ID: usize = 8;
/// The header's length.
pub const HEADER: usize = 8 + 1 + ID + 32 + kem::CIPHERTEXT + 7;
/// What the file key is derived for.
const INFO: &[u8] = b"pgcrypt2 key";

pub const EBADKEY: &str = "bad key file";
pub const EBAD: &str = "not an encrypted file";
pub const ESUITE: &str = "unknown encryption suite";
pub const ENOTFOR: &str = "file is not for this key";

/// A hybrid key pair.
#[derive(Clone)]
pub struct Secret {
    x: Ephemeral,
    kem: kem::Secret,
    bytes: [u8; SECRET],
}

/// Shows the key's id, never its secret.
impl core::fmt::Debug for Secret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Secret({})", hex::encode(&id(&self.public())))
    }
}

impl Secret {
    /// The key pair of 96 secret bytes.
    pub fn from_bytes(bytes: &[u8; SECRET]) -> Secret {
        let mut x = [0u8; 32];
        x.copy_from_slice(&bytes[..32]);
        let mut seed = [0u8; kem::SEED];
        seed.copy_from_slice(&bytes[32..]);
        Secret {
            x: Ephemeral::new(x),
            kem: kem::Secret::from_seed(&seed),
            bytes: *bytes,
        }
    }

    /// A fresh key pair.
    pub fn generate(rng: &mut (impl RngCore + CryptoRng)) -> Secret {
        let mut b = [0u8; SECRET];
        rng.fill_bytes(&mut b);
        Secret::from_bytes(&b)
    }

    pub fn public(&self) -> [u8; PUBLIC] {
        let mut p = [0u8; PUBLIC];
        p[..32].copy_from_slice(&self.x.public);
        p[32..].copy_from_slice(&self.kem.public());
        p
    }

    /// `pgkemkey1 <hex>`, the secret key file's line (with no newline).
    pub fn encode(&self) -> String {
        let mut s = String::from(SECRET_WORD);
        s.push(' ');
        s.push_str(&hex::encode(&self.bytes));
        s
    }

    /// Parses a secret key file: one `pgkemkey1` line, with an optional
    /// newline after it.
    pub fn parse(text: &[u8]) -> Result<Secret, &'static str> {
        let b = parse_line(text, SECRET_WORD, SECRET)?;
        let bytes: [u8; SECRET] = b.try_into().map_err(|_| EBADKEY)?;
        Ok(Secret::from_bytes(&bytes))
    }

    /// The file key a header ([`HEADER`] bytes) carries for this key.
    pub fn open(&self, header: &[u8]) -> Result<[u8; 32], &'static str> {
        let h = Header::parse(header)?;
        let public = self.public();
        if !crate::ct_eq(&h.id, &id(&public)) {
            return Err(ENOTFOR);
        }
        let x = self.x.shared(&h.eph).ok_or(EBAD)?;
        let k = self.kem.decapsulate(h.ct);
        Ok(file_key(&k, &x, header, &public))
    }
}

/// `pgkempub1 <hex>`, a public key's line (with no newline).
pub fn encode_public(public: &[u8; PUBLIC]) -> String {
    let mut s = String::from(PUBLIC_WORD);
    s.push(' ');
    s.push_str(&hex::encode(public));
    s
}

/// Parses a public key: one `pgkempub1` line, with an optional newline
/// after it.
pub fn parse_public(text: &[u8]) -> Result<[u8; PUBLIC], &'static str> {
    parse_line(text, PUBLIC_WORD, PUBLIC)?
        .try_into()
        .map_err(|_| EBADKEY)
}

fn parse_line(text: &[u8], word: &str, len: usize) -> Result<Vec<u8>, &'static str> {
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    let rest = text
        .strip_prefix(word.as_bytes())
        .and_then(|r| r.strip_prefix(b" "))
        .ok_or(EBADKEY)?;
    let b = hex::decode(rest).ok_or(EBADKEY)?;
    if b.len() != len {
        return Err(EBADKEY);
    }
    Ok(b)
}

/// The first [`ID`] bytes of SHA-256 of a public key: names the key a
/// file is for, so the wrong key gets a clear error.
pub fn id(public: &[u8; PUBLIC]) -> [u8; ID] {
    let d = Sha256::digest(public);
    let mut out = [0u8; ID];
    out.copy_from_slice(&d[..ID]);
    out
}

fn file_key(k: &[u8; 32], x: &[u8; 32], header: &[u8], public: &[u8; PUBLIC]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(header);
    h.update(public);
    let salt: [u8; 32] = h.finalize().into();
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(k);
    ikm[32..].copy_from_slice(x);
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut key = [0u8; 32];
    // 32 bytes is always a valid HKDF-SHA256 output length.
    let _ = hk.expand(INFO, &mut key);
    key
}

/// A header taken apart.
pub struct Header<'a> {
    pub id: [u8; ID],
    pub eph: [u8; 32],
    pub ct: &'a [u8; kem::CIPHERTEXT],
    pub prefix: [u8; 7],
}

impl Header<'_> {
    pub fn parse(header: &[u8]) -> Result<Header<'_>, &'static str> {
        if header.len() != HEADER || &header[..8] != MAGIC {
            return Err(EBAD);
        }
        if header[8] != SUITE {
            return Err(ESUITE);
        }
        let mut at = 9;
        let id = header[at..at + ID].try_into().map_err(|_| EBAD)?;
        at += ID;
        let eph = header[at..at + 32].try_into().map_err(|_| EBAD)?;
        at += 32;
        let ct = header[at..at + kem::CIPHERTEXT]
            .try_into()
            .map_err(|_| EBAD)?;
        at += kem::CIPHERTEXT;
        let prefix = header[at..].try_into().map_err(|_| EBAD)?;
        Ok(Header {
            id,
            eph,
            ct,
            prefix,
        })
    }
}

/// What [`seal`] gives: the header, and the key and nonce prefix the
/// chunks are sealed with.
pub type Sealed = (Vec<u8>, [u8; 32], [u8; 7]);

/// A new file for `recipient`: its header, and the key and nonce prefix
/// its chunks are sealed with. Fails when either half of `recipient` is
/// unusable.
pub fn seal(
    recipient: &[u8; PUBLIC],
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<Sealed, &'static str> {
    let mut e = [0u8; 32];
    rng.fill_bytes(&mut e);
    let mut m = [0u8; 32];
    rng.fill_bytes(&mut m);
    let mut prefix = [0u8; 7];
    rng.fill_bytes(&mut prefix);
    seal_with(recipient, e, m, prefix)
}

/// [`seal`] with its randomness given: `e` the ephemeral X25519 secret,
/// `m` the encapsulation's, `prefix` the chunk nonces'.
pub fn seal_with(
    recipient: &[u8; PUBLIC],
    e: [u8; 32],
    m: [u8; 32],
    prefix: [u8; 7],
) -> Result<Sealed, &'static str> {
    let rx: [u8; 32] = recipient[..32].try_into().map_err(|_| EBADKEY)?;
    let ek: &[u8; kem::PUBLIC] = recipient[32..].try_into().map_err(|_| EBADKEY)?;
    let e = Ephemeral::new(e);
    let x = e.shared(&rx).ok_or(EBADKEY)?;
    let (ct, k) = kem::encapsulate(ek, &m).ok_or(EBADKEY)?;
    let mut h = Vec::with_capacity(HEADER);
    h.extend_from_slice(MAGIC);
    h.push(SUITE);
    h.extend_from_slice(&id(recipient));
    h.extend_from_slice(&e.public);
    h.extend_from_slice(&ct);
    h.extend_from_slice(&prefix);
    let key = file_key(&k, &x, &h, recipient);
    Ok((h, key, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login::stream::Cipher;

    #[test]
    fn round_trip() {
        let sk = Secret::from_bytes(&[3; SECRET]);
        let pk = sk.public();
        let (h, key, prefix) = seal_with(&pk, [4; 32], [5; 32], [6; 7]).unwrap();
        assert_eq!(h.len(), HEADER);
        assert_eq!(sk.open(&h).unwrap(), key);
        let mut c = Cipher::with_key(&key, prefix);
        let sealed = c.seal(b"attack at dawn", true).unwrap();
        let mut d = Cipher::with_key(&sk.open(&h).unwrap(), Header::parse(&h).unwrap().prefix);
        assert_eq!(d.open(&sealed[4..], true).unwrap(), b"attack at dawn");
        // Another key: named in the header, so refused before anything.
        let other = Secret::from_bytes(&[7; SECRET]);
        assert_eq!(other.open(&h).unwrap_err(), ENOTFOR);
    }

    /// Every changed byte of the header changes the key (or is refused):
    /// the salt binds all of it.
    #[test]
    fn the_header_is_bound() {
        let sk = Secret::from_bytes(&[3; SECRET]);
        let pk = sk.public();
        let (h, key, _) = seal_with(&pk, [4; 32], [5; 32], [6; 7]).unwrap();
        for i in (0..HEADER).step_by(7).chain([HEADER - 1, 8, 9, 17]) {
            let mut bad = h.clone();
            bad[i] ^= 1;
            if let Ok(k) = sk.open(&bad) {
                assert_ne!(k, key, "byte {i}");
            }
        }
        let mut suite = h.clone();
        suite[8] = 2;
        assert_eq!(sk.open(&suite).unwrap_err(), ESUITE);
        assert_eq!(sk.open(&h[..HEADER - 1]).unwrap_err(), EBAD);
    }

    /// A fixed file key, against values computed separately with Python
    /// (`kyber-py` for ML-KEM-768, `cryptography` for X25519, `hmac` for
    /// HKDF): the derivation is the documented one.
    #[test]
    fn known_answer() {
        let sk = Secret::from_bytes(&[3; SECRET]);
        let pk = sk.public();
        let (h, key, _) = seal_with(&pk, [4; 32], [5; 32], [6; 7]).unwrap();
        let hh = Sha256::digest(&h);
        assert_eq!(hex::encode(&hh), KAT_HEADER);
        assert_eq!(hex::encode(&key), KAT_KEY);
    }

    #[test]
    fn key_files() {
        let sk = Secret::from_bytes(&[3; SECRET]);
        let line = sk.encode();
        assert!(line.starts_with("pgkemkey1 "));
        let back = Secret::parse(alloc::format!("{line}\n").as_bytes()).unwrap();
        assert_eq!(back.public(), sk.public());
        let p = encode_public(&sk.public());
        assert_eq!(parse_public(p.as_bytes()).unwrap(), sk.public());
        assert_eq!(Secret::parse(p.as_bytes()).unwrap_err(), EBADKEY);
        assert_eq!(parse_public(line.as_bytes()).unwrap_err(), EBADKEY);
        assert!(Secret::parse(&line.as_bytes()[..line.len() - 2]).is_err());
        assert!(Secret::parse(line.to_uppercase().as_bytes()).is_err());
    }

    /// A recipient key whose halves are unusable gets no file.
    #[test]
    fn a_bad_recipient_is_refused() {
        let pk = Secret::from_bytes(&[3; SECRET]).public();
        let mut zero = pk;
        zero[..32].fill(0);
        assert_eq!(
            seal_with(&zero, [4; 32], [5; 32], [6; 7]).unwrap_err(),
            EBADKEY
        );
        let mut bad = pk;
        bad[32] = 0xff;
        bad[33] |= 0x0f;
        assert_eq!(
            seal_with(&bad, [4; 32], [5; 32], [6; 7]).unwrap_err(),
            EBADKEY
        );
    }

    const KAT_HEADER: &str = "73bb070e38cdb230310a5c50d2ae0ddfc6e2ee33e83273fadb3af4f32ac83a34";
    const KAT_KEY: &str = "facd4287b2929ca0346fd729719117aca02c1c4f9247f0ac8fddcfcc88f41dac";
}
