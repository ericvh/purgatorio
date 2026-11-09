//! Purgatorio mount authentication: an Ed25519 signature over a server
//! challenge. This is not Inferno's keyring protocol. Real Inferno clients
//! still mount with `-A`.
//!
//! The client writes `signature[64] || public_key[32]`. The server accepts
//! the signature only when that public key is one it already holds.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub const SEED: usize = 32;
pub const SIGNATURE: usize = 64;
pub const PUBLIC: usize = 32;
/// What the client writes to the auth fid.
pub const PROOF: usize = SIGNATURE + PUBLIC;
/// Bytes the server asks the client to sign.
pub const CHALLENGE: usize = 32;

pub const NOT_REQUIRED: &str = "authentication not required";
pub const REQUIRED: &str = "authentication required";
pub const FAILED: &str = "authentication failed";
pub const NO_KEY: &str = "no key";

/// `root/lib/test/auth.key` is this repeated byte. It is a public test
/// fixture, shared by the emu and the probe, not a secret to deploy.
pub const FIXTURE: u8 = 0x42;

pub fn public_key(seed: &[u8; SEED]) -> [u8; PUBLIC] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

pub fn sign(seed: &[u8; SEED], message: &[u8]) -> [u8; SIGNATURE] {
    SigningKey::from_bytes(seed).sign(message).to_bytes()
}

pub fn verify(public: &[u8; PUBLIC], message: &[u8], signature: &[u8; SIGNATURE]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

/// `true` when `proof` is a signature by one of `allowed` over `challenge`.
pub fn accept(allowed: &[[u8; PUBLIC]], challenge: &[u8], proof: &[u8]) -> bool {
    if proof.len() != PROOF {
        return false;
    }
    let Ok(signature) = <[u8; SIGNATURE]>::try_from(&proof[..SIGNATURE]) else {
        return false;
    };
    let Ok(public) = <[u8; PUBLIC]>::try_from(&proof[SIGNATURE..]) else {
        return false;
    };
    allowed.contains(&public) && verify(&public, challenge, &signature)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn a_signature_verifies_for_its_own_key() {
        let seed = [7u8; SEED];
        let public = public_key(&seed);
        let challenge = b"challenge";
        let signature = sign(&seed, challenge);
        assert!(verify(&public, challenge, &signature));
        let mut proof = [0u8; PROOF];
        proof[..SIGNATURE].copy_from_slice(&signature);
        proof[SIGNATURE..].copy_from_slice(&public);
        assert!(accept(&[public], challenge, &proof));
        assert!(!accept(&[[0u8; PUBLIC]], challenge, &proof));
        proof[0] ^= 1;
        assert!(!accept(&[public], challenge, &proof));
    }

    #[test]
    fn the_tree_holds_the_public_fixture() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../root/lib/test");
        let seed = std::fs::read(dir.join("auth.key")).unwrap();
        let public = std::fs::read(dir.join("auth.key.pub")).unwrap();
        assert_eq!(seed, [FIXTURE; SEED]);
        assert_eq!(public, public_key(&[FIXTURE; SEED]));
    }
}
