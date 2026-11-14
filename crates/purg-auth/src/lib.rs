//! Purgatorio's authentication formats and protocols (`docs/auth.md`).
//!
//! - [`cert`]: `pgcert1` certificates, a signer's Ed25519 signature over a
//!   user name, the user's public key and an expiry.
//! - [`keys`]: key lists (`lib/keys/signers`, `lib/keys/revoked`) and
//!   authinfo files.
//! - [`proof`]: the mount handshake's proofs, versions 1 to 4, and the
//!   server's check of them.
//! - [`session`]: the version 3 handshake's ephemeral X25519 exchange,
//!   version 4's hybrid X25519 + ML-KEM-768 exchange, and the session keys
//!   they derive.
//! - [`sig`]: signature keys of either kind, Ed25519 or the hybrid of
//!   Ed25519 and ML-DSA-65, and the typed ids trust lists hold.
//! - [`kem`]: ML-KEM-768 (FIPS 203), the post-quantum half of the hybrid
//!   exchanges.
//! - [`record`]: the ChaCha20-Poly1305 records that carry the Styx
//!   conversation after a version 3 handshake.
//! - [`server`]: the server's proof of a version 3 handshake, which a
//!   client checks to know it reached the server it trusts.
//! - [`login`] (feature `login`): the `pglogin1` password exchange (SPAKE2
//!   over an Argon2id-hardened password) and password-sealed blobs.
//! - [`hybrid`] (feature `login`): `crypt`'s public-key mode, files
//!   encrypted to a hybrid X25519 + ML-KEM-768 key.
//!
//! The crate is `no_std` + `alloc`. The kernel uses only the always-built
//! modules; the guests use all of it.
#![no_std]

extern crate alloc;

pub mod attr;
pub mod cert;
pub mod hex;
#[cfg(feature = "login")]
pub mod hybrid;
pub mod kem;
pub mod keys;
#[cfg(feature = "login")]
pub mod login;
pub mod modsig;
pub mod proof;
pub mod record;
pub mod server;
pub mod session;
pub mod sha256;
pub mod sig;

pub use purg_abi::auth::{PUBLIC, SEED, SIGNATURE};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// The public key of an Ed25519 seed.
pub fn public_key(seed: &[u8; SEED]) -> [u8; PUBLIC] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Signs `message` with `seed`.
pub fn sign(seed: &[u8; SEED], message: &[u8]) -> [u8; SIGNATURE] {
    SigningKey::from_bytes(seed).sign(message).to_bytes()
}

/// Whether `signature` is `public`'s signature over `message`.
pub fn verify(public: &[u8; PUBLIC], message: &[u8], signature: &[u8; SIGNATURE]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    let signature = Signature::from_bytes(signature);
    key.verify_strict(message, &signature).is_ok()
}

/// Whether `a` and `b` are equal, in time that depends only on their
/// lengths.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let d = a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y));
    core::hint::black_box(d) == 0
}
