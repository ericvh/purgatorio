//! ML-KEM-768 (FIPS 203), the post-quantum half of the hybrid key
//! exchange (`docs/auth.md`, "Post-quantum options").
//!
//! It is only ever used beside X25519: the version 4 mount handshake
//! ([`crate::session`]) and `crypt`'s public-key mode ([`crate::hybrid`])
//! feed both shared secrets into one key derivation, so the result is
//! safe while either of the two holds.
//!
//! Randomness comes from the caller, as for [`crate::session::Ephemeral`]:
//! a key pair from a 64-byte seed `d || z`, an encapsulation from 32 bytes
//! `m`. Decapsulation never fails: a ciphertext that was changed gives a
//! pseudo-random secret (FIPS 203's implicit rejection), which then fails
//! whatever was keyed with it.

use ml_kem::array::Array;
use ml_kem::{Decapsulate, KeyExport, MlKem768, ml_kem_768};

pub use purg_abi::auth::{KEM_CIPHERTEXT as CIPHERTEXT, KEM_PUBLIC as PUBLIC};

/// The seed a key pair is made from: FIPS 203's `d || z`.
pub const SEED: usize = 64;
/// A shared secret.
pub const SHARED: usize = 32;

/// An ML-KEM-768 decapsulation key.
#[derive(Clone)]
pub struct Secret {
    dk: ml_kem_768::DecapsulationKey,
}

impl Secret {
    /// The key pair FIPS 203's `ML-KEM.KeyGen_internal(d, z)` makes from
    /// `seed = d || z`.
    pub fn from_seed(seed: &[u8; SEED]) -> Secret {
        Secret {
            dk: ml_kem_768::DecapsulationKey::from_seed(Array::from(*seed)),
        }
    }

    /// The encapsulation key.
    pub fn public(&self) -> [u8; PUBLIC] {
        let ek = self.dk.encapsulation_key().to_bytes();
        let mut out = [0u8; PUBLIC];
        out.copy_from_slice(ek.as_slice());
        out
    }

    /// The secret `ct` carries. A changed `ct` gives a secret nobody else
    /// has.
    pub fn decapsulate(&self, ct: &[u8; CIPHERTEXT]) -> [u8; SHARED] {
        let ct: ml_kem::Ciphertext<MlKem768> = Array::from(*ct);
        let k = self.dk.decapsulate(&ct);
        let mut out = [0u8; SHARED];
        out.copy_from_slice(k.as_slice());
        out
    }
}

/// Encapsulates a fresh secret to `public` with the 32 random bytes `m`
/// (FIPS 203's `ML-KEM.Encaps_internal`). `None` when `public` fails the
/// encapsulation key check of FIPS 203, section 7.2.
pub fn encapsulate(
    public: &[u8; PUBLIC],
    m: &[u8; 32],
) -> Option<([u8; CIPHERTEXT], [u8; SHARED])> {
    let ek = ml_kem_768::EncapsulationKey::new(&Array::from(*public)).ok()?;
    let (ct, k) = ek.encapsulate_deterministic(&Array::from(*m));
    let mut c = [0u8; CIPHERTEXT];
    c.copy_from_slice(ct.as_slice());
    let mut s = [0u8; SHARED];
    s.copy_from_slice(k.as_slice());
    Some((c, s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn hex(b: &[u8]) -> alloc::string::String {
        crate::hex::encode(b)
    }

    /// Known answers computed separately, with the pure-Python `kyber-py`
    /// 1.2 (`ML_KEM_768.key_derive(d || z)`, `_encaps_internal(ek, m)`):
    /// SHA-256 of the encapsulation key and of the ciphertext, and the
    /// shared secret.
    #[test]
    fn known_answer() {
        let mut seed = [0u8; SEED];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        let m = [0x5a; 32];
        let sk = Secret::from_seed(&seed);
        let pk = sk.public();
        assert_eq!(hex(&Sha256::digest(pk)), KAT_EK);
        let (ct, k) = encapsulate(&pk, &m).unwrap();
        assert_eq!(hex(&Sha256::digest(ct)), KAT_CT);
        assert_eq!(hex(&k), KAT_K);
        assert_eq!(sk.decapsulate(&ct), k);
        // Implicit rejection: a changed ciphertext gives another secret,
        // the one FIPS 203 derives from z.
        let mut bad = ct;
        bad[0] ^= 1;
        let r = sk.decapsulate(&bad);
        assert_ne!(r, k);
        assert_eq!(hex(&r), KAT_REJECT);
    }

    /// An encapsulation key whose coefficients are not reduced mod q
    /// fails FIPS 203's check, and gets no encapsulation.
    #[test]
    fn a_bad_public_key_is_refused() {
        let pk = Secret::from_seed(&[1; SEED]).public();
        assert!(encapsulate(&pk, &[2; 32]).is_some());
        let mut bad = pk;
        // The first coefficient becomes 0xfff > q = 3329.
        bad[0] = 0xff;
        bad[1] |= 0x0f;
        assert!(encapsulate(&bad, &[2; 32]).is_none());
    }

    /// Round trips with other seeds.
    #[test]
    fn round_trip() {
        for i in 0..4u8 {
            let sk = Secret::from_seed(&[i; SEED]);
            let (ct, k) = encapsulate(&sk.public(), &[i ^ 0x77; 32]).unwrap();
            assert_eq!(sk.decapsulate(&ct), k);
            let other = Secret::from_seed(&[i + 100; SEED]);
            assert_ne!(other.decapsulate(&ct), k);
        }
    }

    const KAT_EK: &str = "0b7934c83125c788995e2ba6bd761e33046b3e40571be53e023309a29f398cc9";
    const KAT_CT: &str = "fda1ac529e59760ece5aba81b8b16b8428f00911afa89be32ea079351f7190e5";
    const KAT_K: &str = "1345ffad8176a59e2c87a2f2118c7313a6e04ddd06ca857555f6156d4c9f4d95";
    const KAT_REJECT: &str = "29cca2645acf134ef3b019d68739fd5c9ebf9311f3c5604bbd4dba2ef08ae512";
}
