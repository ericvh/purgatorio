//! The session key a version 3 or 4 mount handshake agrees
//! (`docs/auth.md`).
//!
//! Each side makes an ephemeral X25519 key for one conversation. The
//! server offers its public half with the challenge; the client sends its
//! own in the proof, whose signature covers both. The shared secret and a
//! hash of the signed message give two ChaCha20-Poly1305 keys through
//! HKDF-SHA256, one per direction, for the [`crate::record`] layer.
//!
//! Version 4 adds ML-KEM-768 beside X25519 (`docs/auth.md`, "Post-quantum
//! options"): the server also offers an ephemeral encapsulation key, the
//! client sends a ciphertext for it in the proof, and both secrets go into
//! the one key derivation, ML-KEM's first, as X-Wing and TLS's hybrid
//! groups do. The keys are then as strong as the stronger of the two.
//!
//! The ephemeral secrets live only as long as the exchange, so a recorded
//! conversation cannot be read later even with the user's long-term key
//! (forward secrecy).

use alloc::boxed::Box;
use alloc::vec::Vec;

use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::PUBLIC;
use crate::kem;
use crate::proof::{OFFER, OFFER_PQ, SIGNED, SIGNED_PQ};
use crate::record::{Opener, Sealer};

/// HKDF labels of the two directions.
const C2S: &[u8] = b"pgauth3 c2s";
const S2C: &[u8] = b"pgauth3 s2c";
/// HKDF label of the value a server's proof binds to the session
/// ([`crate::server`]).
const BIND: &[u8] = b"pgauth3 server";
/// The same for version 4.
const C2S4: &[u8] = b"pgauth4 c2s";
const S2C4: &[u8] = b"pgauth4 s2c";
const BIND4: &[u8] = b"pgauth4 server";

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

/// The two directions' record keys, and what a server's proof signs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Keys {
    /// Client to server.
    pub c2s: [u8; 32],
    /// Server to client.
    pub s2c: [u8; 32],
    /// SHA-256 of the client's signed message: the challenge, both
    /// ephemeral keys (and for version 4 the encapsulation key and the
    /// ciphertext), both names and the client's identity.
    pub transcript: [u8; 32],
    /// A third HKDF output, `"pgauth3 server"` (`"pgauth4 server"`): only
    /// the two ends of this exchange can compute it, so a server proof
    /// that signs it is good for this session alone.
    pub bind: [u8; 32],
    /// The keys came from the hybrid exchange of a version 4 proof.
    pub pq: bool,
}

impl Keys {
    /// The keys for `shared` and the signed message `transcript`:
    /// HKDF-SHA256 with the transcript's SHA-256 as salt.
    pub fn derive(shared: &[u8; 32], transcript: &[u8]) -> Keys {
        Keys::expand(shared, transcript, [C2S, S2C, BIND], false)
    }

    /// The keys of a version 4 exchange: as [`Keys::derive`], with
    /// `ml_kem || x25519` as the input key material and the version 4
    /// labels. The transcript already holds both public halves, the
    /// encapsulation key and the ciphertext.
    pub fn derive_pq(ml_kem: &[u8; 32], x25519: &[u8; 32], transcript: &[u8]) -> Keys {
        let mut ikm = [0u8; 64];
        ikm[..32].copy_from_slice(ml_kem);
        ikm[32..].copy_from_slice(x25519);
        Keys::expand(&ikm, transcript, [C2S4, S2C4, BIND4], true)
    }

    fn expand(ikm: &[u8], transcript: &[u8], labels: [&[u8]; 3], pq: bool) -> Keys {
        let salt: [u8; 32] = Sha256::digest(transcript).into();
        let hk = Hkdf::<Sha256>::new(Some(&salt), ikm);
        let mut k = Keys {
            c2s: [0; 32],
            s2c: [0; 32],
            transcript: salt,
            bind: [0; 32],
            pq,
        };
        // 32 bytes is always a valid HKDF-SHA256 output length.
        let _ = hk.expand(labels[0], &mut k.c2s);
        let _ = hk.expand(labels[1], &mut k.s2c);
        let _ = hk.expand(labels[2], &mut k.bind);
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

/// The server's side of one exchange: its challenge and ephemeral key,
/// and once a client asked for it, its ephemeral ML-KEM-768 key.
#[derive(Clone)]
pub struct Server {
    key: Ephemeral,
    /// `challenge[32] || public[32]`, read from the auth fid.
    pub offer: [u8; OFFER],
    pq: Option<Box<ServerPq>>,
}

#[derive(Clone)]
struct ServerPq {
    kem: kem::Secret,
    /// [`Server::offer`] followed by the encapsulation key.
    offer: Vec<u8>,
}

impl Server {
    pub fn new(challenge: [u8; 32], random: [u8; 32]) -> Server {
        let key = Ephemeral::new(random);
        let mut offer = [0u8; OFFER];
        offer[..32].copy_from_slice(&challenge);
        offer[32..].copy_from_slice(&key.public);
        Server {
            key,
            offer,
            pq: None,
        }
    }

    /// Makes the ephemeral ML-KEM-768 key from 64 random bytes, unless
    /// there is one already: the first call decides, so every read of
    /// the long offer gives the same key.
    pub fn make_pq(&mut self, seed: &[u8; kem::SEED]) {
        if self.pq.is_some() {
            return;
        }
        let kem = kem::Secret::from_seed(seed);
        let mut offer = Vec::with_capacity(OFFER_PQ);
        offer.extend_from_slice(&self.offer);
        offer.extend_from_slice(&kem.public());
        self.pq = Some(Box::new(ServerPq { kem, offer }));
    }

    /// The [`OFFER_PQ`] bytes, once [`Server::make_pq`] has made the key.
    pub fn offer_pq(&self) -> Option<&[u8]> {
        self.pq.as_ref().map(|p| p.offer.as_slice())
    }

    /// Everything this server offered: the [`OFFER_PQ`] when a client
    /// read it, else the [`OFFER`]. What [`crate::proof::check`] takes.
    pub fn offered(&self) -> &[u8] {
        self.offer_pq().unwrap_or(&self.offer)
    }

    /// The session keys for a version 3 or 4 `proof` written for this
    /// offer. `None` when `proof` is neither, its keys are unusable, or
    /// it is version 4 and no client read the long offer. This does not
    /// check the proof: [`crate::proof::check`] does.
    pub fn keys(&self, uname: &str, aname: &str, proof: &[u8]) -> Option<Keys> {
        let t = crate::proof::transcript(self.offered(), uname, aname, proof)?;
        let x = self.key.shared(&t.client_key)?;
        match t.ct {
            None => Some(Keys::derive(&x, &t.message)),
            Some(ct) => {
                let k = self.pq.as_ref()?.kem.decapsulate(&ct);
                Some(Keys::derive_pq(&k, &x, &t.message))
            }
        }
    }
}

/// The client's side of one exchange.
#[derive(Clone)]
pub struct Client {
    key: Ephemeral,
    /// What the proof signs: the server's offer followed by this client's
    /// public key ([`SIGNED`] bytes), and for version 4 its ML-KEM-768
    /// ciphertext ([`SIGNED_PQ`] bytes).
    pub signed: Vec<u8>,
    /// The ML-KEM-768 secret of a version 4 exchange.
    kem: Option<[u8; kem::SHARED]>,
}

impl Client {
    /// Answers the server's `offer`. `None` when its key is unusable.
    pub fn new(offer: &[u8; OFFER], random: [u8; 32]) -> Option<Client> {
        let key = Ephemeral::new(random);
        let server: [u8; PUBLIC] = offer[32..].try_into().ok()?;
        key.shared(&server)?;
        Some(Client {
            signed: crate::proof::signed(offer, &key.public).to_vec(),
            key,
            kem: None,
        })
    }

    /// Answers a server's [`OFFER_PQ`] for a version 4 exchange, with 32
    /// random bytes for the X25519 key and 32 for the encapsulation.
    /// `None` when either of its keys is unusable.
    pub fn new_pq(offer: &[u8], random: [u8; 32], m: [u8; 32]) -> Option<Client> {
        let offer: &[u8; OFFER_PQ] = offer.try_into().ok()?;
        let key = Ephemeral::new(random);
        let server: [u8; PUBLIC] = offer[32..OFFER].try_into().ok()?;
        key.shared(&server)?;
        let ek: &[u8; kem::PUBLIC] = offer[OFFER..].try_into().ok()?;
        let (ct, k) = kem::encapsulate(ek, &m)?;
        Some(Client {
            signed: crate::proof::signed_pq(offer, &key.public, &ct),
            key,
            kem: Some(k),
        })
    }

    /// Whether this is a version 4 exchange.
    pub fn pq(&self) -> bool {
        self.kem.is_some()
    }

    /// The session keys once `proof`, made over [`Client::signed`], is
    /// sent. `None` when `proof` is not of this exchange's version or
    /// does not carry this client's key (and ciphertext).
    pub fn keys(&self, uname: &str, aname: &str, proof: &[u8]) -> Option<Keys> {
        let (offer, len) = match self.kem {
            None => (&self.signed[..OFFER], SIGNED),
            Some(_) => (&self.signed[..OFFER_PQ], SIGNED_PQ),
        };
        let t = crate::proof::transcript(offer, uname, aname, proof)?;
        if t.client_key != self.key.public || t.ct.is_some() != self.kem.is_some() {
            return None;
        }
        if let Some(ct) = &t.ct
            && ct[..] != self.signed[OFFER_PQ + PUBLIC..len]
        {
            return None;
        }
        let server: [u8; PUBLIC] = offer[32..OFFER].try_into().ok()?;
        let x = self.key.shared(&server)?;
        Some(match &self.kem {
            None => Keys::derive(&x, &t.message),
            Some(k) => Keys::derive_pq(k, &x, &t.message),
        })
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

    /// The version 4 derivation, against values computed separately with
    /// Python's `hmac` (HKDF-SHA256, salt SHA-256("t"), input
    /// `ml_kem || x25519`).
    #[test]
    fn derive_pq_vector() {
        let k = Keys::derive_pq(&[2; 32], &[1; 32], b"t");
        assert!(k.pq);
        assert_eq!(
            crate::hex::encode(&k.c2s),
            "d9620c68e6cd4fb80c4123cdb685b3e3401203053c3ae2fad9423c15ea6f34f1"
        );
        assert_eq!(
            crate::hex::encode(&k.s2c),
            "eabd77b18945fbc0d8babdf2393886b4b697b200f7e5074839b6398c829b5d6d"
        );
        assert_eq!(
            crate::hex::encode(&k.bind),
            "b74227f996d3291c2c8012dfda7b04aac1bd7c64fe1f029bdf84af258e9cad31"
        );
        // Either secret changes every key; so does the order.
        assert_ne!(Keys::derive_pq(&[3; 32], &[1; 32], b"t").c2s, k.c2s);
        assert_ne!(Keys::derive_pq(&[2; 32], &[4; 32], b"t").c2s, k.c2s);
        assert_ne!(Keys::derive_pq(&[1; 32], &[2; 32], b"t").c2s, k.c2s);
    }

    /// A version 4 exchange: both sides agree, on keys that differ from a
    /// version 3 exchange's with the same X25519 keys.
    #[test]
    fn exchange_pq() {
        let seed = [1u8; 32];
        let mut server = Server::new([9; 32], [4; 32]);
        assert!(server.offer_pq().is_none());
        assert_eq!(server.offered().len(), OFFER);
        server.make_pq(&[7; kem::SEED]);
        let offer = server.offer_pq().unwrap().to_vec();
        assert_eq!(offer.len(), OFFER_PQ);
        assert_eq!(&offer[..OFFER], &server.offer);
        // A second call keeps the first key.
        server.make_pq(&[8; kem::SEED]);
        assert_eq!(server.offer_pq().unwrap(), &offer[..]);
        let client = Client::new_pq(&offer, [5; 32], [6; 32]).unwrap();
        assert!(client.pq());
        assert_eq!(client.signed.len(), SIGNED_PQ);
        let proof = crate::proof::make(&seed, None, &client.signed, "alice", "");
        assert_eq!(proof[0], crate::proof::V4);
        let c = client.keys("alice", "", &proof).unwrap();
        let s = server.keys("alice", "", &proof).unwrap();
        assert_eq!(c, s);
        assert!(c.pq);
        // The same X25519 keys in a version 3 exchange agree other keys.
        let v3 = Client::new(&server.offer, [5; 32]).unwrap();
        let p3 = crate::proof::make(&seed, None, &v3.signed, "alice", "");
        let k3 = server.keys("alice", "", &p3).unwrap();
        assert!(!k3.pq);
        assert_ne!(k3.c2s, c.c2s);
        // A changed ciphertext gives the server other keys (and the
        // signature over it no longer verifies, see `proof`).
        let mut bad = proof.clone();
        bad[2 + 64 + 32] ^= 1;
        assert_ne!(server.keys("alice", "", &bad).unwrap(), c);
        assert!(client.keys("alice", "", &bad).is_none());
        // A version 4 proof to a server that never made the long offer.
        let plain = Server::new([9; 32], [4; 32]);
        assert!(plain.keys("alice", "", &proof).is_none());
        // A version 3 proof is not this client's, nor a v4 one a v3 client's.
        assert!(client.keys("alice", "", &p3).is_none());
        assert!(v3.keys("alice", "", &proof).is_none());
    }

    /// A long offer whose ML-KEM key is malformed, or that is cut short,
    /// gets no answer.
    #[test]
    fn a_bad_pq_offer_is_refused() {
        let mut server = Server::new([9; 32], [4; 32]);
        server.make_pq(&[7; kem::SEED]);
        let offer = server.offer_pq().unwrap().to_vec();
        assert!(Client::new_pq(&offer[..OFFER], [5; 32], [6; 32]).is_none());
        let mut bad = offer.clone();
        bad[OFFER] = 0xff;
        bad[OFFER + 1] |= 0x0f;
        assert!(Client::new_pq(&bad, [5; 32], [6; 32]).is_none());
        let mut zero = offer;
        zero[32..OFFER].fill(0);
        assert!(Client::new_pq(&zero, [5; 32], [6; 32]).is_none());
    }
}
