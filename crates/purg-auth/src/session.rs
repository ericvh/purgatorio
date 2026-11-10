//! The session key a version 3 mount handshake agrees (`docs/auth.md`).
//!
//! Each side makes an ephemeral X25519 key for one conversation. The
//! server offers its public half with the challenge; the client sends its
//! own in the proof, whose signature covers both. The shared secret and a
//! hash of the signed message give two ChaCha20-Poly1305 keys through
//! HKDF-SHA256, one per direction, for the [`crate::record`] layer.
//!
//! The ephemeral secrets live only as long as the exchange, so a recorded
//! conversation cannot be read later even with the user's long-term key
//! (forward secrecy).

use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::PUBLIC;
use crate::proof::{OFFER, SIGNED};
use crate::record::{Opener, Sealer};

/// HKDF labels of the two directions.
const C2S: &[u8] = b"pgauth3 c2s";
const S2C: &[u8] = b"pgauth3 s2c";

/// An ephemeral X25519 key pair.
#[derive(Clone)]
pub struct Ephemeral {
    secret: [u8; 32],
    /// The public half.
    pub public: [u8; PUBLIC],
}

impl Ephemeral {
    /// A key pair from 32 random bytes.
    pub fn new(random: [u8; 32]) -> Ephemeral {
        Ephemeral {
            public: MontgomeryPoint::mul_base_clamped(random).to_bytes(),
            secret: random,
        }
    }

    /// The secret shared with the holder of `peer`. `None` for a peer key
    /// of small order, which would make it all zeros.
    pub fn shared(&self, peer: &[u8; PUBLIC]) -> Option<[u8; 32]> {
        let s = MontgomeryPoint(*peer).mul_clamped(self.secret).to_bytes();
        (s != [0u8; 32]).then_some(s)
    }
}

/// The two directions' record keys.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Keys {
    /// Client to server.
    pub c2s: [u8; 32],
    /// Server to client.
    pub s2c: [u8; 32],
}

impl Keys {
    /// The keys for `shared` and the signed message `transcript`:
    /// HKDF-SHA256 with the transcript's SHA-256 as salt.
    pub fn derive(shared: &[u8; 32], transcript: &[u8]) -> Keys {
        let salt = Sha256::digest(transcript);
        let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
        let mut k = Keys {
            c2s: [0; 32],
            s2c: [0; 32],
        };
        // 32 bytes is always a valid HKDF-SHA256 output length.
        let _ = hk.expand(C2S, &mut k.c2s);
        let _ = hk.expand(S2C, &mut k.s2c);
        k
    }

    /// The client's ends: it seals with `c2s` and opens with `s2c`.
    pub fn client(&self) -> (Sealer, Opener) {
        (Sealer::new(&self.c2s), Opener::new(&self.s2c))
    }

    /// The server's ends.
    pub fn server(&self) -> (Sealer, Opener) {
        (Sealer::new(&self.s2c), Opener::new(&self.c2s))
    }
}

/// The server's side of one exchange: its challenge and ephemeral key.
#[derive(Clone)]
pub struct Server {
    key: Ephemeral,
    /// `challenge[32] || public[32]`, read from the auth fid.
    pub offer: [u8; OFFER],
}

impl Server {
    pub fn new(challenge: [u8; 32], random: [u8; 32]) -> Server {
        let key = Ephemeral::new(random);
        let mut offer = [0u8; OFFER];
        offer[..32].copy_from_slice(&challenge);
        offer[32..].copy_from_slice(&key.public);
        Server { key, offer }
    }

    /// The session keys for a version 3 `proof` written for this offer.
    /// `None` when `proof` is not version 3 or its key is unusable. This
    /// does not check the proof: [`crate::proof::check`] does.
    pub fn keys(&self, uname: &str, aname: &str, proof: &[u8]) -> Option<Keys> {
        let (transcript, client) = crate::proof::transcript(&self.offer, uname, aname, proof)?;
        let shared = self.key.shared(&client)?;
        Some(Keys::derive(&shared, &transcript))
    }
}

/// The client's side of one exchange.
#[derive(Clone)]
pub struct Client {
    key: Ephemeral,
    /// The server's offer followed by this client's public key: what the
    /// proof signs.
    pub signed: [u8; SIGNED],
}

impl Client {
    /// Answers the server's `offer`. `None` when its key is unusable.
    pub fn new(offer: &[u8; OFFER], random: [u8; 32]) -> Option<Client> {
        let key = Ephemeral::new(random);
        let server: [u8; PUBLIC] = offer[32..].try_into().ok()?;
        key.shared(&server)?;
        Some(Client {
            signed: crate::proof::signed(offer, &key.public),
            key,
        })
    }

    /// The session keys once `proof`, made over [`Client::signed`], is
    /// sent. `None` when `proof` is not version 3 or does not carry this
    /// client's key.
    pub fn keys(&self, uname: &str, aname: &str, proof: &[u8]) -> Option<Keys> {
        let offer: [u8; OFFER] = self.signed[..OFFER].try_into().ok()?;
        let (transcript, client) = crate::proof::transcript(&offer, uname, aname, proof)?;
        if client != self.key.public {
            return None;
        }
        let server: [u8; PUBLIC] = offer[32..].try_into().ok()?;
        let shared = self.key.shared(&server)?;
        Some(Keys::derive(&shared, &transcript))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7748, section 6.1.
    #[test]
    fn x25519_vector() {
        let a = crate::hex::array::<32>(
            b"77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        )
        .unwrap();
        let b = crate::hex::array::<32>(
            b"5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
        )
        .unwrap();
        let a = Ephemeral::new(a);
        let b = Ephemeral::new(b);
        assert_eq!(
            crate::hex::encode(&a.public),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        assert_eq!(
            crate::hex::encode(&b.public),
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
        );
        let k = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
        assert_eq!(crate::hex::encode(&a.shared(&b.public).unwrap()), k);
        assert_eq!(crate::hex::encode(&b.shared(&a.public).unwrap()), k);
        // A small-order point gives no secret.
        assert!(a.shared(&[0u8; 32]).is_none());
    }

    /// Both sides of an exchange agree, the keys differ per direction, and
    /// a different transcript gives different keys.
    #[test]
    fn exchange() {
        let seed = [1u8; 32];
        let server = Server::new([9; 32], [4; 32]);
        let client = Client::new(&server.offer, [5; 32]).unwrap();
        let proof = crate::proof::make(&seed, None, &client.signed, "alice", "");
        let c = client.keys("alice", "", &proof).unwrap();
        let s = server.keys("alice", "", &proof).unwrap();
        assert_eq!(c, s);
        assert_ne!(c.c2s, c.s2c);
        assert_ne!(server.keys("bob", "", &proof).unwrap(), c);
        // A proof that carries another client key.
        let other = Client::new(&server.offer, [6; 32]).unwrap();
        let p2 = crate::proof::make(&seed, None, &other.signed, "alice", "");
        assert!(client.keys("alice", "", &p2).is_none());
        // A version 2 proof agrees nothing.
        let p2 = crate::proof::make(&seed, None, &server.offer[..32], "alice", "");
        assert!(server.keys("alice", "", &p2).is_none());
        // The derivation itself, fixed.
        let k = Keys::derive(&[1; 32], b"t");
        assert_eq!(k, Keys::derive(&[1; 32], b"t"));
        assert_ne!(k, Keys::derive(&[1; 32], b"u"));
    }
}
