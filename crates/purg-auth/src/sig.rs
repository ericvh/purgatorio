//! Signature keys of either kind: Ed25519, or the hybrid of Ed25519 and
//! ML-DSA-65 (FIPS 204), where both signatures must verify (`docs/auth.md`,
//! "Post-quantum signatures").
//!
//! ```text
//! hybrid secret    = ed25519 seed[32] || ml-dsa-65 seed xi[32]
//! hybrid public    = ed25519 public[32] || ml-dsa-65 public[1952]
//! hybrid signature = ed25519 sig[64] || ml-dsa-65 sig[3309]
//! both halves sign   "pghybrid1" 0x00 || message
//! hybrid id        = SHA-256("pghybrid1 key" 0x00 || hybrid public)
//! ```
//!
//! ML-DSA signs deterministically (FIPS 204's optional variant, with an
//! empty context string), so a key and a message give one signature and
//! no random source is needed where a signature is made.
//!
//! In text a hybrid key is `mldsa65:` and the hex of its bytes, and an
//! Ed25519 key is its bare hex, as before. A trust list holds a hybrid key
//! by its id behind the same marker ([`Id`]); the marker keeps the id from
//! being read as an Ed25519 key, which a quantum attacker could otherwise
//! find a secret for (an id is a valid Ed25519 point about half the
//! time).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use ml_dsa::{
    EncodedSignature, EncodedVerifyingKey, Keypair, MlDsa65, Signature, SigningKey, VerifyingKey,
};
use sha2::{Digest, Sha256};

use crate::{PUBLIC, SEED, SIGNATURE, hex};

pub use purg_abi::auth::{HYBRID_PUBLIC, HYBRID_SEED, HYBRID_SIGNATURE};

/// What a hybrid key or id starts with in text.
pub const MARKER: &str = "mldsa65:";
/// What both halves of a hybrid signature sign before the message, so
/// neither half is a signature of the message on its own.
pub const CONTEXT: &[u8] = b"pghybrid1\0";
/// What a hybrid key's id hashes before the key.
pub const ID_CONTEXT: &[u8] = b"pghybrid1 key\0";
/// An ML-DSA-65 public key.
pub const MLDSA_PUBLIC: usize = HYBRID_PUBLIC - PUBLIC;
/// An ML-DSA-65 signature.
pub const MLDSA_SIGNATURE: usize = HYBRID_SIGNATURE - SIGNATURE;

/// The ML-DSA-65 key of a 32-byte seed.
fn mldsa(xi: &[u8; 32]) -> SigningKey<MlDsa65> {
    SigningKey::<MlDsa65>::from_seed(&(*xi).into())
}

/// The ML-DSA-65 public key of a 32-byte seed.
pub fn mldsa_public(xi: &[u8; 32]) -> [u8; MLDSA_PUBLIC] {
    let mut out = [0u8; MLDSA_PUBLIC];
    out.copy_from_slice(&mldsa(xi).verifying_key().encode());
    out
}

/// `xi`'s deterministic ML-DSA-65 signature over `message`, with an
/// empty context string.
pub fn mldsa_sign(xi: &[u8; 32], message: &[u8]) -> [u8; MLDSA_SIGNATURE] {
    let mut out = [0u8; MLDSA_SIGNATURE];
    // Only a context string longer than 255 bytes fails; this one is empty.
    if let Ok(s) = mldsa(xi).expanded_key().sign_deterministic(message, b"") {
        out.copy_from_slice(&s.encode());
    }
    out
}

/// Whether `signature` is `public`'s ML-DSA-65 signature over `message`.
pub fn mldsa_verify(public: &[u8; MLDSA_PUBLIC], message: &[u8], signature: &[u8]) -> bool {
    let Ok(enc) = EncodedVerifyingKey::<MlDsa65>::try_from(&public[..]) else {
        return false;
    };
    let Ok(sig) = EncodedSignature::<MlDsa65>::try_from(signature) else {
        return false;
    };
    let Some(sig) = Signature::<MlDsa65>::decode(&sig) else {
        return false;
    };
    VerifyingKey::<MlDsa65>::decode(&enc).verify_with_context(message, b"", &sig)
}

/// `CONTEXT || message`, what both halves of a hybrid signature sign.
fn framed(message: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTEXT.len() + message.len());
    m.extend_from_slice(CONTEXT);
    m.extend_from_slice(message);
    m
}

/// A hybrid secret key: an Ed25519 seed and an ML-DSA-65 seed.
#[derive(Clone, PartialEq, Eq)]
pub struct HybridSecret(pub [u8; HYBRID_SEED]);

impl core::fmt::Debug for HybridSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("HybridSecret(..)")
    }
}

impl HybridSecret {
    /// The Ed25519 half.
    pub fn ed25519(&self) -> [u8; SEED] {
        let mut s = [0u8; SEED];
        s.copy_from_slice(&self.0[..SEED]);
        s
    }

    /// The ML-DSA-65 half.
    pub fn xi(&self) -> [u8; 32] {
        let mut s = [0u8; 32];
        s.copy_from_slice(&self.0[SEED..]);
        s
    }

    /// The public key.
    pub fn public(&self) -> HybridPublic {
        let mut p = Box::new([0u8; HYBRID_PUBLIC]);
        p[..PUBLIC].copy_from_slice(&crate::public_key(&self.ed25519()));
        p[PUBLIC..].copy_from_slice(&mldsa_public(&self.xi()));
        HybridPublic(p)
    }

    /// Signs `message`: both halves over `CONTEXT || message`.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let m = framed(message);
        let mut s = Vec::with_capacity(HYBRID_SIGNATURE);
        s.extend_from_slice(&crate::sign(&self.ed25519(), &m));
        s.extend_from_slice(&mldsa_sign(&self.xi(), &m));
        s
    }
}

/// A hybrid public key: Ed25519's 32 bytes, then ML-DSA-65's 1952.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HybridPublic(pub Box<[u8; HYBRID_PUBLIC]>);

impl HybridPublic {
    /// From its bytes. `None` for the wrong length.
    pub fn from_slice(b: &[u8]) -> Option<HybridPublic> {
        let a: [u8; HYBRID_PUBLIC] = b.try_into().ok()?;
        Some(HybridPublic(Box::new(a)))
    }

    /// The id a trust list names it by.
    pub fn id(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(ID_CONTEXT);
        h.update(&self.0[..]);
        h.finalize().into()
    }

    /// Whether `signature` is this key's hybrid signature over `message`:
    /// both halves must verify.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        if signature.len() != HYBRID_SIGNATURE {
            return false;
        }
        let m = framed(message);
        let mut ed = [0u8; PUBLIC];
        ed.copy_from_slice(&self.0[..PUBLIC]);
        let mut ml = [0u8; MLDSA_PUBLIC];
        ml.copy_from_slice(&self.0[PUBLIC..]);
        let mut es = [0u8; SIGNATURE];
        es.copy_from_slice(&signature[..SIGNATURE]);
        // Both are always checked, so the time does not say which failed.
        let a = crate::verify(&ed, &m, &es);
        let b = mldsa_verify(&ml, &m, &signature[SIGNATURE..]);
        a & b
    }

    /// `mldsa65:` and the hex of the key.
    pub fn encode(&self) -> String {
        let mut s = String::from(MARKER);
        s.push_str(&hex::encode(&self.0[..]));
        s
    }

    /// Parses [`HybridPublic::encode`]'s form.
    pub fn parse(text: &[u8]) -> Option<HybridPublic> {
        let h = text.strip_prefix(MARKER.as_bytes())?;
        if h.len() != HYBRID_PUBLIC * 2 {
            return None;
        }
        let v = hex::decode(h)?;
        HybridPublic::from_slice(&v)
    }
}

/// A secret key of either kind.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Secret {
    Ed25519([u8; SEED]),
    Hybrid(HybridSecret),
}

impl Secret {
    /// A key from a seed file's bytes: 32 for Ed25519, 64 for a hybrid
    /// key. `None` for any other length.
    pub fn from_seed_file(b: &[u8]) -> Option<Secret> {
        match b.len() {
            SEED => Some(Secret::Ed25519(b.try_into().ok()?)),
            HYBRID_SEED => Some(Secret::Hybrid(HybridSecret(b.try_into().ok()?))),
            _ => None,
        }
    }

    /// The bytes of its seed file.
    pub fn seed_bytes(&self) -> &[u8] {
        match self {
            Secret::Ed25519(s) => s,
            Secret::Hybrid(h) => &h.0,
        }
    }

    pub fn is_hybrid(&self) -> bool {
        matches!(self, Secret::Hybrid(_))
    }

    pub fn public(&self) -> Public {
        match self {
            Secret::Ed25519(s) => Public::Ed25519(crate::public_key(s)),
            Secret::Hybrid(h) => Public::Hybrid(h.public()),
        }
    }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        match self {
            Secret::Ed25519(s) => crate::sign(s, message).to_vec(),
            Secret::Hybrid(h) => h.sign(message),
        }
    }

    /// Hex of the seed, behind [`MARKER`] for a hybrid key.
    pub fn encode(&self) -> String {
        match self {
            Secret::Ed25519(s) => hex::encode(s),
            Secret::Hybrid(h) => {
                let mut s = String::from(MARKER);
                s.push_str(&hex::encode(&h.0));
                s
            }
        }
    }

    /// Parses [`Secret::encode`]'s form.
    pub fn parse(text: &[u8]) -> Option<Secret> {
        match text.strip_prefix(MARKER.as_bytes()) {
            Some(h) => Some(Secret::Hybrid(HybridSecret(hex::array::<HYBRID_SEED>(h)?))),
            None => Some(Secret::Ed25519(hex::array::<SEED>(text)?)),
        }
    }
}

/// A public key of either kind.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Public {
    Ed25519([u8; PUBLIC]),
    Hybrid(HybridPublic),
}

impl Public {
    /// From its bytes: 32 for Ed25519, [`HYBRID_PUBLIC`] for a hybrid key.
    pub fn from_slice(b: &[u8]) -> Option<Public> {
        match b.len() {
            PUBLIC => Some(Public::Ed25519(b.try_into().ok()?)),
            HYBRID_PUBLIC => Some(Public::Hybrid(HybridPublic::from_slice(b)?)),
            _ => None,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Public::Ed25519(k) => k,
            Public::Hybrid(h) => &h.0[..],
        }
    }

    pub fn is_hybrid(&self) -> bool {
        matches!(self, Public::Hybrid(_))
    }

    /// How a trust list names it.
    pub fn id(&self) -> Id {
        match self {
            Public::Ed25519(k) => Id::Ed25519(*k),
            Public::Hybrid(h) => Id::Hybrid(h.id()),
        }
    }

    /// The length of this kind's signatures.
    pub fn signature_len(&self) -> usize {
        if self.is_hybrid() {
            HYBRID_SIGNATURE
        } else {
            SIGNATURE
        }
    }

    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self {
            Public::Ed25519(k) => signature
                .try_into()
                .is_ok_and(|s: [u8; SIGNATURE]| crate::verify(k, message, &s)),
            Public::Hybrid(h) => h.verify(message, signature),
        }
    }

    /// Hex, behind [`MARKER`] for a hybrid key.
    pub fn encode(&self) -> String {
        match self {
            Public::Ed25519(k) => hex::encode(k),
            Public::Hybrid(h) => h.encode(),
        }
    }

    /// Parses [`Public::encode`]'s form.
    pub fn parse(text: &[u8]) -> Option<Public> {
        if text.starts_with(MARKER.as_bytes()) {
            return HybridPublic::parse(text).map(Public::Hybrid);
        }
        Some(Public::Ed25519(hex::array::<PUBLIC>(text)?))
    }
}

/// How a trust list (`/lib/keys/signers`, `users`, `servers`, `revoked`)
/// names a key: an Ed25519 key itself, or a hybrid key's id. The two
/// never match each other, so a listed hybrid id accepts only hybrid
/// proofs and a listed Ed25519 key only Ed25519 ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Id {
    Ed25519([u8; PUBLIC]),
    Hybrid([u8; 32]),
}

impl Id {
    /// Hex, behind [`MARKER`] for a hybrid id.
    pub fn encode(&self) -> String {
        match self {
            Id::Ed25519(k) => hex::encode(k),
            Id::Hybrid(i) => {
                let mut s = String::from(MARKER);
                s.push_str(&hex::encode(i));
                s
            }
        }
    }

    /// Parses [`Id::encode`]'s form: 64 hex digits, with or without the
    /// marker.
    pub fn parse(text: &[u8]) -> Option<Id> {
        match text.strip_prefix(MARKER.as_bytes()) {
            Some(h) => Some(Id::Hybrid(hex::array::<32>(h)?)),
            None => Some(Id::Ed25519(hex::array::<PUBLIC>(text)?)),
        }
    }

    pub fn is_hybrid(&self) -> bool {
        matches!(self, Id::Hybrid(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> HybridSecret {
        let mut s = [0u8; HYBRID_SEED];
        for (i, b) in s.iter_mut().enumerate() {
            *b = (i % 32) as u8;
        }
        HybridSecret(s)
    }

    fn sha(b: &[u8]) -> String {
        hex::encode(&<[u8; 32]>::from(Sha256::digest(b)))
    }

    /// FIPS 204 ML-DSA-65 key generation from the seed 00 01 .. 1f: the
    /// public key of the IETF LAMPS example (draft-ietf-lamps-dilithium-
    /// certificates, `ML-DSA-65-seed.priv`), which `dilithium-py` 1.5.1
    /// also gives. Its deterministic signature over `"pghybrid1" 0x00 ||
    /// "purgatorio"` with an empty context is `dilithium-py`'s
    /// (`ML_DSA_65.sign(sk, m, deterministic=True)`), not this code's.
    #[test]
    fn fips204_vectors() {
        let xi = seed().xi();
        let pk = mldsa_public(&xi);
        assert_eq!(
            sha(&pk),
            "d666806e11cee19a7c989f7445f90dd419cf4d2d51db8c0fdb4c0f0a542238c9"
        );
        assert_eq!(hex::encode(&pk[..8]), "48683d91978e31eb");
        let m = framed(b"purgatorio");
        let sig = mldsa_sign(&xi, &m);
        assert_eq!(
            sha(&sig),
            "36b0d949a020bb5ecf9d9a2047b49b2da8da82ea7bb57621d7eec8d5ff85d75f"
        );
        assert_eq!(hex::encode(&sig[..16]), "bf1fc637034d985bfe3141d143bfef1b");
        assert!(mldsa_verify(&pk, &m, &sig));
        let mut bad = sig;
        bad[100] ^= 1;
        assert!(!mldsa_verify(&pk, &m, &bad));
    }

    #[test]
    fn hybrid_sign_and_verify() {
        let k = seed();
        let p = k.public();
        let s = k.sign(b"message");
        assert_eq!(s.len(), HYBRID_SIGNATURE);
        assert!(p.verify(b"message", &s));
        assert!(!p.verify(b"massage", &s));
        // Either half alone breaks it.
        for i in [3, SIGNATURE + 7, HYBRID_SIGNATURE - 1] {
            let mut t = s.clone();
            t[i] ^= 1;
            assert!(!p.verify(b"message", &t), "{i}");
        }
        assert!(!p.verify(b"message", &s[..SIGNATURE]));
        // The Ed25519 half signs the framed message, so it is not an
        // Ed25519 signature of the message itself.
        let ed: [u8; SIGNATURE] = s[..SIGNATURE].try_into().unwrap();
        let edp: [u8; PUBLIC] = p.0[..PUBLIC].try_into().unwrap();
        assert!(!crate::verify(&edp, b"message", &ed));
        assert!(crate::verify(&edp, &framed(b"message"), &ed));
        // A key with another ML-DSA half does not verify it.
        let mut other = k.0;
        other[40] ^= 1;
        assert!(!HybridSecret(other).public().verify(b"message", &s));
    }

    #[test]
    fn text_forms() {
        let k = Secret::Hybrid(seed());
        let p = k.public();
        assert_eq!(Secret::parse(k.encode().as_bytes()), Some(k.clone()));
        assert_eq!(Public::parse(p.encode().as_bytes()), Some(p.clone()));
        assert!(p.encode().starts_with(MARKER));
        let id = p.id();
        assert!(id.is_hybrid());
        assert_eq!(Id::parse(id.encode().as_bytes()), Some(id));
        // The id with its marker taken off is an Ed25519 entry, which
        // never equals the hybrid one.
        let bare = &id.encode()[MARKER.len()..];
        let ed = Id::parse(bare.as_bytes()).unwrap();
        assert!(!ed.is_hybrid());
        assert_ne!(ed, id);
        let e = Secret::Ed25519([7; SEED]);
        assert_eq!(Secret::parse(e.encode().as_bytes()), Some(e.clone()));
        assert_eq!(
            Public::parse(e.public().encode().as_bytes()),
            Some(e.public())
        );
        assert_eq!(Secret::from_seed_file(k.seed_bytes()), Some(k));
        assert_eq!(Secret::from_seed_file(&[7; SEED]), Some(e));
        assert_eq!(Secret::from_seed_file(&[7; 33]), None);
        assert!(Public::parse(b"mldsa65:00").is_none());
    }
}
