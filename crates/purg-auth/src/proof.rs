//! Mount proofs. The client writes one in a single write to the auth fid:
//!
//! - version 1, exactly [`V1_LEN`] bytes: `signature || public_key` over
//!   the raw challenge;
//! - version 2: `0x02 || kind || signature[64] || body`, where the body is
//!   a public key (kind 0) or a `pgcert1` line (kind 1), and the signature
//!   is over [`message`];
//! - version 3: `0x03 || kind || signature[64] || client_key[32] || body`,
//!   which also carries the client's ephemeral X25519 key. The signature
//!   covers the server's and the client's ephemeral keys, and both sides
//!   derive the session keys from them ([`crate::session`]);
//! - version 4: `0x04 || kind || signature[64] || client_key[32] ||
//!   ciphertext[1088] || body`, version 3 with an ML-KEM-768 ciphertext
//!   for the server's ephemeral encapsulation key beside it: the hybrid,
//!   post-quantum exchange (`docs/auth.md`, "Post-quantum options").
//!
//! Versions 3 and 4 also take kinds 2 (a hybrid Ed25519 + ML-DSA-65 key)
//! and 3 (a `pgcert2` certificate of one), whose signature is a hybrid one
//! of [`HYBRID_SIGNATURE`] bytes instead of 64 (`docs/auth.md`,
//! "Post-quantum signatures"). The signed messages do not change.
//!
//! What the client signs is the "challenge" it was given: the server's 32
//! random bytes for version 2, for version 3 [`SIGNED`] bytes, the
//! server's [`OFFER`] (challenge and ephemeral key) followed by the
//! client's ephemeral key, and for version 4 [`SIGNED_PQ`] bytes, the
//! server's [`OFFER_PQ`] followed by the client's key and ciphertext.
//! [`message`], [`assemble`] and [`make`] pick the version from that
//! length.

use alloc::vec::Vec;

use purg_abi::auth::{
    CHALLENGE, EXPIRED, FAILED, KIND_CERT, KIND_HYBRID_CERT, KIND_HYBRID_KEY, KIND_KEY,
    NAME_MISMATCH, PQSIG_REQUIRED, PROOF, PROOF_VERSION, REVOKED, UNTRUSTED,
};

use crate::cert::{AnyCert, Cert};
use crate::sig::{HYBRID_SIGNATURE, Id, Public, Secret};
use crate::{PUBLIC, SEED, SIGNATURE};

/// The length of a version 1 proof.
pub const V1_LEN: usize = PROOF;
/// The first byte of a version 3 proof.
pub const V3: u8 = purg_abi::auth::PROOF_V3;
/// The first byte of a version 4 proof.
pub const V4: u8 = purg_abi::auth::PROOF_V4;
/// The context string a version 2 signature starts with.
pub const CONTEXT: &[u8] = b"pgauth2\0";
/// The context string a version 3 signature starts with.
pub const CONTEXT3: &[u8] = b"pgauth3\0";
/// The context string a version 4 signature starts with.
pub const CONTEXT4: &[u8] = b"pgauth4\0";
/// What a version 3 server gives on the auth fid: `challenge[32] ||
/// server_key[32]`, its ephemeral X25519 key.
pub const OFFER: usize = purg_abi::auth::OFFER;
/// What a version 3 client signs: the [`OFFER`] and its own ephemeral key.
pub const SIGNED: usize = purg_abi::auth::SIGNED;
/// What a server gives for a long read of the auth fid: the [`OFFER`]
/// and its ephemeral ML-KEM-768 encapsulation key.
pub const OFFER_PQ: usize = purg_abi::auth::OFFER_PQ;
/// What a version 4 client signs: the [`OFFER_PQ`], its ephemeral X25519
/// key and its ML-KEM-768 ciphertext.
pub const SIGNED_PQ: usize = purg_abi::auth::SIGNED_PQ;
/// An ML-KEM-768 ciphertext.
pub const CT: usize = purg_abi::auth::KEM_CIPHERTEXT;
/// A proof is at most this long: a version 4 proof with a `pgcert2`
/// certificate that has every signature fits, and so does one message
/// of the largest `msize` (64 KiB of data).
pub const MAX_LEN: usize = 60 * 1024;

/// The length of a proof's signature for `kind`: 64 bytes for an
/// Ed25519 key or certificate, [`HYBRID_SIGNATURE`] for a hybrid one.
pub fn signature_len(kind: u8) -> Option<usize> {
    match kind {
        KIND_KEY | KIND_CERT => Some(SIGNATURE),
        KIND_HYBRID_KEY | KIND_HYBRID_CERT => Some(HYBRID_SIGNATURE),
        _ => None,
    }
}

/// The bytes a version 2, 3 or 4 proof signs. `challenge` is the
/// server's challenge (version 2), [`SIGNED`] bytes (version 3) or
/// [`SIGNED_PQ`] bytes (version 4).
pub fn message(challenge: &[u8], uname: &str, aname: &str, kind: u8, body: &[u8]) -> Vec<u8> {
    let context = match challenge.len() {
        SIGNED => CONTEXT3,
        SIGNED_PQ => CONTEXT4,
        _ => CONTEXT,
    };
    let mut m = Vec::with_capacity(context.len() + challenge.len() + 5 + body.len() + 64);
    m.extend_from_slice(context);
    m.extend_from_slice(challenge);
    for s in [uname, aname] {
        let n = u16::try_from(s.len()).unwrap_or(u16::MAX);
        m.extend_from_slice(&n.to_le_bytes());
        m.extend_from_slice(&s.as_bytes()[..n as usize]);
    }
    m.push(kind);
    m.extend_from_slice(body);
    m
}

/// The body of a proof: the key itself, or a certificate line.
pub fn body(cert: Option<&Cert>, public: &[u8; PUBLIC]) -> (u8, Vec<u8>) {
    match cert {
        Some(c) => (KIND_CERT, c.encode().into_bytes()),
        None => (KIND_KEY, public.to_vec()),
    }
}

/// [`body`] for a key of either kind: kind 0 or 1 for Ed25519, 2 or 3
/// for a hybrid key.
pub fn body_any(cert: Option<&AnyCert>, public: &Public) -> (u8, Vec<u8>) {
    match (cert, public) {
        (Some(c @ AnyCert::V1(_)), _) => (KIND_CERT, c.encode().into_bytes()),
        (Some(c @ AnyCert::V2(_)), _) => (KIND_HYBRID_CERT, c.encode().into_bytes()),
        (None, Public::Ed25519(k)) => (KIND_KEY, k.to_vec()),
        (None, Public::Hybrid(h)) => (KIND_HYBRID_KEY, h.0.to_vec()),
    }
}

/// Assembles a proof from its parts: version 3 when `challenge` is
/// [`SIGNED`] bytes (the client's key is its last 32), version 4 when it
/// is [`SIGNED_PQ`] bytes (the client's key and ciphertext follow the
/// [`OFFER_PQ`]), else version 2.
pub fn assemble(challenge: &[u8], kind: u8, signature: &[u8], body: &[u8]) -> Vec<u8> {
    let (version, tail) = match challenge.len() {
        SIGNED => (V3, &challenge[OFFER..]),
        SIGNED_PQ => (V4, &challenge[OFFER_PQ..]),
        _ => (PROOF_VERSION, &[][..]),
    };
    let mut p = Vec::with_capacity(2 + signature.len() + tail.len() + body.len());
    p.push(version);
    p.push(kind);
    p.extend_from_slice(signature);
    p.extend_from_slice(tail);
    p.extend_from_slice(body);
    p
}

/// A proof signed with `seed`, carrying `cert` when given: version 3 when
/// `challenge` is [`SIGNED`] bytes, version 4 for [`SIGNED_PQ`] bytes,
/// else version 2.
pub fn make(
    seed: &[u8; SEED],
    cert: Option<&Cert>,
    challenge: &[u8],
    uname: &str,
    aname: &str,
) -> Vec<u8> {
    let (kind, body) = body(cert, &crate::public_key(seed));
    let sig = crate::sign(seed, &message(challenge, uname, aname, kind, &body));
    assemble(challenge, kind, &sig, &body)
}

/// [`make`] with a key of either kind. A hybrid key makes a kind 2 or 3
/// proof, which only versions 3 and 4 carry: with a bare 32-byte
/// challenge it is signed as version 2 and no server takes it.
pub fn make_any(
    secret: &Secret,
    cert: Option<&AnyCert>,
    challenge: &[u8],
    uname: &str,
    aname: &str,
) -> Vec<u8> {
    let (kind, body) = body_any(cert, &secret.public());
    let sig = secret.sign(&message(challenge, uname, aname, kind, &body));
    assemble(challenge, kind, &sig, &body)
}

/// A version 3 or 4 proof taken apart.
pub struct V3Proof<'a> {
    pub kind: u8,
    /// 64 bytes, or [`HYBRID_SIGNATURE`] for kinds 2 and 3.
    pub signature: &'a [u8],
    /// The client's ephemeral X25519 key.
    pub client_key: [u8; PUBLIC],
    /// The client's ML-KEM-768 ciphertext: version 4 only.
    pub ct: Option<&'a [u8; CT]>,
    pub body: &'a [u8],
}

impl V3Proof<'_> {
    /// Whether this is a version 4 proof, with the post-quantum exchange.
    pub fn pq(&self) -> bool {
        self.ct.is_some()
    }
}

/// Splits a version 3 proof. `None` when it is not one.
pub fn parse_v3(proof: &[u8]) -> Option<V3Proof<'_>> {
    if proof.first() != Some(&V3) {
        return None;
    }
    parse_session(proof)
}

/// Splits a version 4 proof. `None` when it is not one.
pub fn parse_v4(proof: &[u8]) -> Option<V3Proof<'_>> {
    if proof.first() != Some(&V4) {
        return None;
    }
    parse_session(proof)
}

/// Splits a version 3 or 4 proof. `None` when it is neither.
pub fn parse_session(proof: &[u8]) -> Option<V3Proof<'_>> {
    let pq = match proof.first() {
        Some(&V3) => false,
        Some(&V4) => true,
        _ => return None,
    };
    if proof.len() > MAX_LEN {
        return None;
    }
    let kind = *proof.get(1)?;
    let n = signature_len(kind)?;
    let signature = proof.get(2..2 + n)?;
    let mut at = 2 + n;
    let client_key = proof.get(at..at + PUBLIC)?.try_into().ok()?;
    at += PUBLIC;
    let ct = if pq {
        let ct: &[u8; CT] = proof.get(at..at + CT)?.try_into().ok()?;
        at += CT;
        Some(ct)
    } else {
        None
    };
    let body = &proof[at..];
    if body.is_empty() {
        return None;
    }
    Some(V3Proof {
        kind,
        signature,
        client_key,
        ct,
        body,
    })
}

/// The [`SIGNED`] bytes of a version 3 exchange.
pub fn signed(offer: &[u8; OFFER], client_key: &[u8; PUBLIC]) -> [u8; SIGNED] {
    let mut s = [0u8; SIGNED];
    s[..OFFER].copy_from_slice(offer);
    s[OFFER..].copy_from_slice(client_key);
    s
}

/// The [`SIGNED_PQ`] bytes of a version 4 exchange.
pub fn signed_pq(offer: &[u8; OFFER_PQ], client_key: &[u8; PUBLIC], ct: &[u8; CT]) -> Vec<u8> {
    let mut s = Vec::with_capacity(SIGNED_PQ);
    s.extend_from_slice(offer);
    s.extend_from_slice(client_key);
    s.extend_from_slice(ct);
    s
}

/// The bytes a version 3 or 4 proof `p` signs, given the server's
/// `offer`: for version 3 its first [`OFFER`] bytes, for version 4 all
/// [`OFFER_PQ`] of them. `None` when `offer` does not fit `p`.
fn signed_for(offer: &[u8], p: &V3Proof<'_>) -> Option<Vec<u8>> {
    match p.ct {
        None => {
            let offer: &[u8; OFFER] = offer.get(..OFFER)?.try_into().ok()?;
            Some(signed(offer, &p.client_key).to_vec())
        }
        Some(ct) => {
            let offer: &[u8; OFFER_PQ] = offer.try_into().ok()?;
            Some(signed_pq(offer, &p.client_key, ct))
        }
    }
}

/// What a version 3 or 4 proof signs, and the client's half of the
/// exchange it carries.
pub struct Transcript {
    /// The signed message. Both sides hash it into the session keys.
    pub message: Vec<u8>,
    /// The client's ephemeral X25519 key.
    pub client_key: [u8; PUBLIC],
    /// The client's ML-KEM-768 ciphertext: version 4 only.
    pub ct: Option<[u8; CT]>,
}

/// The message a version 3 or 4 `proof` signs, given what the server
/// offered (`offer`: the [`OFFER`], or for version 4 the [`OFFER_PQ`]),
/// and the client's half of the exchange it carries. `None` when `proof`
/// is neither version or `offer` does not fit it.
pub fn transcript(offer: &[u8], uname: &str, aname: &str, proof: &[u8]) -> Option<Transcript> {
    let p = parse_session(proof)?;
    let s = signed_for(offer, &p)?;
    Some(Transcript {
        message: message(&s, uname, aname, p.kind, p.body),
        client_key: p.client_key,
        ct: p.ct.copied(),
    })
}

/// What a server trusts. Keys and signers are typed ([`Id`]): a listed
/// Ed25519 key accepts only Ed25519 proofs, a listed hybrid id only
/// hybrid ones.
pub struct Policy<'a> {
    /// Keys the server holds directly.
    pub keys: &'a [Id],
    /// Trusted signers.
    pub signers: &'a [Id],
    /// Revoked keys. `None` when the list did not parse: every
    /// certificate then fails.
    pub revoked: Option<&'a [Id]>,
    /// Seconds since 1970 UTC.
    pub now: u64,
    /// Accept only hybrid keys and certificates (`-C mldsa65`): anything
    /// else fails with [`PQSIG_REQUIRED`].
    pub hybrid: bool,
}

impl Policy<'_> {
    /// Whether this policy can accept anything at all.
    pub fn requires_auth(&self) -> bool {
        !self.keys.is_empty() || !self.signers.is_empty()
    }

    fn revoked(&self, key: &Id) -> bool {
        self.revoked.is_some_and(|r| r.contains(key))
    }
}

/// Checks `proof` against `policy` for this `Tauth`. Returns the error the
/// write fails with.
///
/// `offer` is what the server gave on the auth fid: the 32-byte challenge,
/// or the [`OFFER`] of a server that takes version 3. Versions 1 and 2
/// sign the challenge, its first 32 bytes.
pub fn check(
    policy: &Policy<'_>,
    offer: &[u8],
    uname: &str,
    aname: &str,
    proof: &[u8],
) -> Result<(), &'static str> {
    let challenge = offer.get(..CHALLENGE).unwrap_or(offer);
    if proof.len() == V1_LEN {
        if policy.hybrid {
            return Err(PQSIG_REQUIRED);
        }
        let (sig, key) = proof.split_at(SIGNATURE);
        let sig: [u8; SIGNATURE] = sig.try_into().map_err(|_| FAILED)?;
        let key: [u8; PUBLIC] = key.try_into().map_err(|_| FAILED)?;
        let id = Id::Ed25519(key);
        if !policy.keys.contains(&id) || !crate::verify(&key, challenge, &sig) {
            return Err(FAILED);
        }
        return if policy.revoked(&id) {
            Err(REVOKED)
        } else {
            Ok(())
        };
    }
    let (kind, sig, body, signed) = if matches!(proof.first(), Some(&V3) | Some(&V4)) {
        let p = parse_session(proof).ok_or(FAILED)?;
        let s = signed_for(offer, &p).ok_or(FAILED)?;
        let m = message(&s, uname, aname, p.kind, p.body);
        (p.kind, p.signature, p.body, m)
    } else {
        // Version 2 carries Ed25519 proofs only.
        if proof.len() < 2 + SIGNATURE
            || proof.len() > MAX_LEN
            || proof[0] != PROOF_VERSION
            || !matches!(proof[1], KIND_KEY | KIND_CERT)
        {
            return Err(FAILED);
        }
        let kind = proof[1];
        let sig = &proof[2..2 + SIGNATURE];
        let body = &proof[2 + SIGNATURE..];
        (
            kind,
            sig,
            body,
            message(challenge, uname, aname, kind, body),
        )
    };
    check_signed(policy, kind, sig, body, &signed, Some(uname)).map(|_| ())
}

/// Checks a proof's `kind`, `signature` and `body` over the message
/// `signed` against `policy`: the key must be one the policy holds
/// directly, or the body a certificate it takes (for `uname`, when
/// given). Returns the prover's key. Shared by the client's proof and
/// the server's ([`crate::server`]).
pub fn check_signed(
    policy: &Policy<'_>,
    kind: u8,
    signature: &[u8],
    body: &[u8],
    signed: &[u8],
    uname: Option<&str>,
) -> Result<Public, &'static str> {
    match kind {
        KIND_KEY | KIND_HYBRID_KEY => {
            let key = Public::from_slice(body).ok_or(FAILED)?;
            if key.is_hybrid() != (kind == KIND_HYBRID_KEY) {
                return Err(FAILED);
            }
            if policy.hybrid && !key.is_hybrid() {
                return Err(PQSIG_REQUIRED);
            }
            let id = key.id();
            if !policy.keys.contains(&id) || !key.verify(signed, signature) {
                return Err(FAILED);
            }
            if policy.revoked(&id) {
                return Err(REVOKED);
            }
            Ok(key)
        }
        KIND_CERT | KIND_HYBRID_CERT => {
            let cert = AnyCert::parse(body).map_err(|_| FAILED)?;
            if cert.is_hybrid() != (kind == KIND_HYBRID_CERT) {
                return Err(FAILED);
            }
            if policy.hybrid && !cert.is_hybrid() {
                return Err(PQSIG_REQUIRED);
            }
            let key = cert.key();
            if !key.verify(signed, signature) {
                return Err(FAILED);
            }
            check_cert(policy, &cert, uname)?;
            Ok(key)
        }
        _ => Err(FAILED),
    }
}

/// Checks a certificate against `policy`, as a server does once the
/// proof's signature verifies. `uname`, when given, must be its name.
pub fn check_cert(
    policy: &Policy<'_>,
    cert: &AnyCert,
    uname: Option<&str>,
) -> Result<(), &'static str> {
    if policy.hybrid && !cert.is_hybrid() {
        return Err(PQSIG_REQUIRED);
    }
    let key = cert.key().id();
    if policy.revoked.is_none() || policy.revoked(&key) {
        return Err(REVOKED);
    }
    if policy.keys.contains(&key) {
        return Ok(());
    }
    if uname.is_some_and(|u| u != cert.name()) {
        return Err(NAME_MISMATCH);
    }
    if cert.expired(policy.now) {
        return Err(EXPIRED);
    }
    // `signed_by` takes only a signer of the certificate's own kind, so a
    // listed Ed25519 key never vouches for a hybrid certificate, nor a
    // listed hybrid id for an Ed25519 one.
    let signers = cert.signers();
    let trusted = |s: &Id| policy.signers.contains(s) && cert.signed_by(s);
    if signers.iter().any(|s| trusted(s) && !policy.revoked(s)) {
        return Ok(());
    }
    if signers.iter().any(trusted) {
        return Err(REVOKED);
    }
    Err(UNTRUSTED)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const USER: [u8; SEED] = [1; SEED];
    const SIGNER: [u8; SEED] = [2; SEED];
    const CHAL: [u8; 32] = [7; 32];
    const NOW: u64 = 1_700_000_000;

    fn cert(name: &str, expires: u64, signer: &[u8; SEED]) -> Cert {
        let mut c = Cert::new(name, crate::public_key(&USER), expires).unwrap();
        c.sign(signer).unwrap();
        c
    }

    /// Ed25519 keys as trust entries, leaked for the test's life.
    pub(crate) fn ids(keys: &[[u8; PUBLIC]]) -> &'static [Id] {
        alloc::boxed::Box::leak(
            keys.iter()
                .map(Id::from)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    fn policy(
        keys: &[[u8; PUBLIC]],
        signers: &[[u8; PUBLIC]],
        revoked: &[[u8; PUBLIC]],
    ) -> Policy<'static> {
        Policy {
            keys: ids(keys),
            signers: ids(signers),
            revoked: Some(ids(revoked)),
            now: NOW,
            hybrid: false,
        }
    }

    #[test]
    fn version_1_is_for_direct_keys() {
        let public = crate::public_key(&USER);
        let mut p = crate::sign(&USER, &CHAL).to_vec();
        p.extend_from_slice(&public);
        assert_eq!(
            check(&policy(&[public], &[], &[]), &CHAL, "u", "", &p),
            Ok(())
        );
        let signer = [crate::public_key(&SIGNER)];
        assert_eq!(
            check(&policy(&[], &signer, &[]), &CHAL, "u", "", &p),
            Err(FAILED)
        );
        assert_eq!(
            check(&policy(&[public], &[], &[public]), &CHAL, "u", "", &p),
            Err(REVOKED)
        );
        p[0] ^= 1;
        assert_eq!(
            check(&policy(&[public], &[], &[]), &CHAL, "u", "", &p),
            Err(FAILED)
        );
    }

    #[test]
    fn version_2_direct_key() {
        let public = crate::public_key(&USER);
        let p = make(&USER, None, &CHAL, "alice", "");
        assert_ne!(p.len(), V1_LEN);
        let pol = [public];
        assert_eq!(
            check(&policy(&pol, &[], &[]), &CHAL, "alice", "", &p),
            Ok(())
        );
        // The signature covers the challenge, uname and aname.
        assert_eq!(
            check(&policy(&pol, &[], &[]), &[8; 32], "alice", "", &p),
            Err(FAILED)
        );
        assert_eq!(
            check(&policy(&pol, &[], &[]), &CHAL, "bob", "", &p),
            Err(FAILED)
        );
        assert_eq!(
            check(&policy(&pol, &[], &[]), &CHAL, "alice", "x", &p),
            Err(FAILED)
        );
        assert_eq!(
            check(&policy(&[], &[], &[]), &CHAL, "alice", "", &p),
            Err(FAILED)
        );
    }

    #[test]
    fn version_2_certificate() {
        let signer = [crate::public_key(&SIGNER)];
        let good = cert("alice", NOW + 60, &SIGNER);
        let pol = policy(&[], &signer, &[]);
        let p = make(&USER, Some(&good), &CHAL, "alice", "");
        assert_eq!(check(&pol, &CHAL, "alice", "", &p), Ok(()));
        // Replay on another challenge.
        assert_eq!(check(&pol, &[0; 32], "alice", "", &p), Err(FAILED));
        // Someone else's certificate.
        let p = make(&USER, Some(&good), &CHAL, "bob", "");
        assert_eq!(check(&pol, &CHAL, "bob", "", &p), Err(NAME_MISMATCH));
        // Expired.
        let old = cert("alice", NOW, &SIGNER);
        let p = make(&USER, Some(&old), &CHAL, "alice", "");
        assert_eq!(check(&pol, &CHAL, "alice", "", &p), Err(EXPIRED));
        // Never expires.
        let forever = cert("alice", 0, &SIGNER);
        let p = make(&USER, Some(&forever), &CHAL, "alice", "");
        assert_eq!(check(&pol, &CHAL, "alice", "", &p), Ok(()));
        // An untrusted signer.
        let other = cert("alice", NOW + 60, &[3; SEED]);
        let p = make(&USER, Some(&other), &CHAL, "alice", "");
        assert_eq!(check(&pol, &CHAL, "alice", "", &p), Err(UNTRUSTED));
        // Signed by someone holding a different key than the certificate's.
        let p = make(&[5; SEED], Some(&good), &CHAL, "alice", "");
        assert_eq!(check(&pol, &CHAL, "alice", "", &p), Err(FAILED));
    }

    #[test]
    fn revocation() {
        let signer = [crate::public_key(&SIGNER)];
        let user = [crate::public_key(&USER)];
        let good = cert("alice", NOW + 60, &SIGNER);
        let p = make(&USER, Some(&good), &CHAL, "alice", "");
        assert_eq!(
            check(&policy(&[], &signer, &user), &CHAL, "alice", "", &p),
            Err(REVOKED)
        );
        assert_eq!(
            check(&policy(&[], &signer, &signer), &CHAL, "alice", "", &p),
            Err(REVOKED)
        );
        let broken = Policy {
            keys: &[],
            signers: ids(&signer),
            revoked: None,
            now: NOW,
            hybrid: false,
        };
        assert_eq!(check(&broken, &CHAL, "alice", "", &p), Err(REVOKED));
        // A countersignature by a second trusted signer still counts.
        let mut both = good.clone();
        both.sign(&[6; SEED]).unwrap();
        let two = [signer[0], crate::public_key(&[6; SEED])];
        let p = make(&USER, Some(&both), &CHAL, "alice", "");
        assert_eq!(
            check(&policy(&[], &two, &signer), &CHAL, "alice", "", &p),
            Ok(())
        );
    }

    #[test]
    fn a_certificate_for_a_held_key_needs_no_signer() {
        let user = [crate::public_key(&USER)];
        let other = cert("bob", 1, &[3; SEED]);
        let p = make(&USER, Some(&other), &CHAL, "alice", "");
        assert_eq!(
            check(&policy(&user, &[], &[]), &CHAL, "alice", "", &p),
            Ok(())
        );
    }

    /// A version 3 proof signs both ephemeral keys: a server key or a
    /// client key changed on the way makes it fail.
    #[test]
    fn version_3() {
        let public = [crate::public_key(&USER)];
        let signer = [crate::public_key(&SIGNER)];
        let mut offer = [0u8; OFFER];
        offer[..32].copy_from_slice(&CHAL);
        offer[32..].copy_from_slice(&[8; 32]);
        let s = signed(&offer, &[9; 32]);
        let p = make(&USER, None, &s, "alice", "");
        assert_eq!(p[0], V3);
        assert_eq!(&p[2 + SIGNATURE..2 + SIGNATURE + PUBLIC], &[9; 32]);
        let pol = policy(&public, &[], &[]);
        assert_eq!(check(&pol, &offer, "alice", "", &p), Ok(()));
        // Checked against a version 2 server's bare challenge.
        assert_eq!(check(&pol, &CHAL, "alice", "", &p), Err(FAILED));
        // The server's key replaced.
        let mut other = offer;
        other[40] ^= 1;
        assert_eq!(check(&pol, &other, "alice", "", &p), Err(FAILED));
        // The client's key replaced.
        let mut q = p.clone();
        q[2 + SIGNATURE] ^= 1;
        assert_eq!(check(&pol, &offer, "alice", "", &q), Err(FAILED));
        // uname and aname are still covered.
        assert_eq!(check(&pol, &offer, "bob", "", &p), Err(FAILED));
        assert_eq!(check(&pol, &offer, "alice", "x", &p), Err(FAILED));
        // A certificate.
        let good = cert("alice", NOW + 60, &SIGNER);
        let p = make(&USER, Some(&good), &s, "alice", "");
        assert_eq!(
            check(&policy(&[], &signer, &[]), &offer, "alice", "", &p),
            Ok(())
        );
        let t = transcript(&offer, "alice", "", &p).unwrap();
        assert_eq!(t.client_key, [9; 32]);
        assert!(t.ct.is_none());
        assert!(t.message.starts_with(CONTEXT3));
        // A v2 proof is no transcript, and a v3 proof is not a v2 proof.
        let v2 = make(&USER, None, &CHAL, "alice", "");
        assert!(transcript(&offer, "alice", "", &v2).is_none());
        let mut as_v2 = make(&USER, None, &s, "alice", "");
        as_v2[0] = PROOF_VERSION;
        assert_eq!(check(&pol, &offer, "alice", "", &as_v2), Err(FAILED));
    }

    /// A version 4 proof signs the whole long offer and the ciphertext,
    /// under its own context: nothing can be changed or cut out of it,
    /// and it cannot pass as a version 3 proof.
    #[test]
    fn version_4() {
        let public = [crate::public_key(&USER)];
        let signer = [crate::public_key(&SIGNER)];
        let mut offer = alloc::vec![0u8; OFFER_PQ];
        offer[..32].copy_from_slice(&CHAL);
        offer[32..OFFER].copy_from_slice(&[8; 32]);
        offer[OFFER..].fill(0x33);
        let ct = [0x44u8; CT];
        let s = signed_pq(offer.as_slice().try_into().unwrap(), &[9; 32], &ct);
        let p = make(&USER, None, &s, "alice", "");
        assert_eq!(p[0], V4);
        assert_eq!(&p[2 + SIGNATURE..2 + SIGNATURE + PUBLIC], &[9; 32]);
        assert_eq!(&p[2 + SIGNATURE + PUBLIC..2 + SIGNATURE + PUBLIC + CT], &ct);
        let v = parse_v4(&p).unwrap();
        assert!(v.pq());
        assert!(parse_v3(&p).is_none());
        let pol = policy(&public, &[], &[]);
        assert_eq!(check(&pol, &offer, "alice", "", &p), Ok(()));
        // Against the short offer only: the encapsulation key is missing.
        assert_eq!(check(&pol, &offer[..OFFER], "alice", "", &p), Err(FAILED));
        // The server's encapsulation key changed on the way.
        let mut other = offer.clone();
        other[OFFER + 100] ^= 1;
        assert_eq!(check(&pol, &other, "alice", "", &p), Err(FAILED));
        // The ciphertext changed.
        let mut q = p.clone();
        q[2 + SIGNATURE + PUBLIC + 5] ^= 1;
        assert_eq!(check(&pol, &offer, "alice", "", &q), Err(FAILED));
        // Stripped: the ciphertext cut out and the version made 3, as a
        // downgrade to the classical exchange would need.
        let mut stripped = p[..2 + SIGNATURE + PUBLIC].to_vec();
        stripped[0] = V3;
        stripped.extend_from_slice(&p[2 + SIGNATURE + PUBLIC + CT..]);
        assert!(parse_v3(&stripped).is_some());
        assert_eq!(check(&pol, &offer, "alice", "", &stripped), Err(FAILED));
        // A version 3 proof still checks against a server that made the
        // long offer: it signs the first OFFER bytes.
        let short: &[u8; OFFER] = offer[..OFFER].try_into().unwrap();
        let p3 = make(&USER, None, &signed(short, &[9; 32]), "alice", "");
        assert_eq!(check(&pol, &offer, "alice", "", &p3), Ok(()));
        // Names, certificates and the transcript as for version 3.
        assert_eq!(check(&pol, &offer, "bob", "", &p), Err(FAILED));
        let good = cert("alice", NOW + 60, &SIGNER);
        let pc = make(&USER, Some(&good), &s, "alice", "");
        assert_eq!(
            check(&policy(&[], &signer, &[]), &offer, "alice", "", &pc),
            Ok(())
        );
        let t = transcript(&offer, "alice", "", &pc).unwrap();
        assert_eq!(t.ct, Some(ct));
        assert!(t.message.starts_with(CONTEXT4));
        // Cut short anywhere before the body.
        let head = 2 + SIGNATURE + PUBLIC + CT;
        for n in [1, 2 + SIGNATURE, head - 1, head] {
            assert_eq!(check(&pol, &offer, "alice", "", &p[..n]), Err(FAILED));
        }
    }

    fn hybrid(n: u8) -> crate::sig::HybridSecret {
        crate::sig::HybridSecret([n; crate::sig::HYBRID_SEED])
    }

    fn hcert(name: &str, user: u8, signer: u8) -> AnyCert {
        let mut c = crate::cert::Cert2::new(name, hybrid(user).public(), NOW + 60).unwrap();
        c.sign(&hybrid(signer)).unwrap();
        AnyCert::V2(c)
    }

    fn typed(keys: &[Id], signers: &[Id], hybrid: bool) -> Policy<'static> {
        Policy {
            keys: alloc::boxed::Box::leak(keys.to_vec().into_boxed_slice()),
            signers: alloc::boxed::Box::leak(signers.to_vec().into_boxed_slice()),
            revoked: Some(&[]),
            now: NOW,
            hybrid,
        }
    }

    /// A version 3 exchange's signed bytes.
    fn signed3() -> Vec<u8> {
        signed(&[7; OFFER], &[9; 32]).to_vec()
    }

    #[test]
    fn hybrid_key_proofs() {
        let user = Secret::Hybrid(hybrid(1));
        let public = user.public();
        let id = public.id();
        let s = signed3();
        let p = make_any(&user, None, &s, "alice", "");
        assert_eq!(p[1], KIND_HYBRID_KEY);
        let parsed = parse_v3(&p).unwrap();
        assert_eq!(parsed.signature.len(), HYBRID_SIGNATURE);
        assert_eq!(parsed.client_key, [9; 32]);
        assert_eq!(parsed.body, public.bytes());
        let offer = &s[..OFFER];
        assert_eq!(
            check(&typed(&[id], &[], false), offer, "alice", "", &p),
            Ok(())
        );
        assert_eq!(
            check(&typed(&[id], &[], true), offer, "alice", "", &p),
            Ok(())
        );
        // The id's 32 bytes listed as an Ed25519 key do not take it.
        let Id::Hybrid(raw) = id else { panic!() };
        assert_eq!(
            check(
                &typed(&[Id::Ed25519(raw)], &[], false),
                offer,
                "alice",
                "",
                &p
            ),
            Err(FAILED)
        );
        // Either half of the signature changed.
        for at in [2 + 5, 2 + SIGNATURE + 5] {
            let mut q = p.clone();
            q[at] ^= 1;
            assert_eq!(
                check(&typed(&[id], &[], false), offer, "alice", "", &q),
                Err(FAILED)
            );
        }
        // Wrong kind for the body.
        let mut q = p.clone();
        q[1] = KIND_KEY;
        assert_eq!(
            check(&typed(&[id], &[], false), offer, "alice", "", &q),
            Err(FAILED)
        );
        // Version 2 does not carry hybrid proofs.
        let p2 = make_any(&user, None, &CHAL, "alice", "");
        assert_eq!(
            check(&typed(&[id], &[], false), &CHAL, "alice", "", &p2),
            Err(FAILED)
        );
        // Version 4 carries them too.
        let offer4 = [5u8; OFFER_PQ];
        let s4 = signed_pq(&offer4, &[9; 32], &[3; CT]);
        let p4 = make_any(&user, None, &s4, "alice", "");
        assert!(parse_v4(&p4).unwrap().pq());
        assert_eq!(
            check(&typed(&[id], &[], true), &offer4, "alice", "", &p4),
            Ok(())
        );
    }

    /// Stripping the ML-DSA half: what is left is an Ed25519 signature
    /// over the hybrid framing, not over the proof's message, and the
    /// listed hybrid id does not take an Ed25519 key anyway.
    #[test]
    fn stripped_hybrid_proofs_fail() {
        let h = hybrid(1);
        let user = Secret::Hybrid(h.clone());
        let s = signed3();
        let offer = &s[..OFFER];
        let p = make_any(&user, None, &s, "alice", "");
        let ed_pub = crate::public_key(&h.ed25519());
        let mut stripped = alloc::vec![V3, KIND_KEY];
        stripped.extend_from_slice(&p[2..2 + SIGNATURE]);
        stripped.extend_from_slice(&[9; 32]);
        stripped.extend_from_slice(&ed_pub);
        // Even a server that lists the Ed25519 half refuses it: the
        // signature is over "pghybrid1" 0x00 || message.
        let both = [user.public().id(), Id::Ed25519(ed_pub)];
        assert_eq!(
            check(&typed(&both, &[], false), offer, "alice", "", &stripped),
            Err(FAILED)
        );
        assert_eq!(
            check(
                &typed(&[user.public().id()], &[], false),
                offer,
                "alice",
                "",
                &stripped
            ),
            Err(FAILED)
        );
        // A server that requires hybrid says so.
        assert_eq!(
            check(&typed(&both, &[], true), offer, "alice", "", &stripped),
            Err(PQSIG_REQUIRED)
        );
        // A hybrid certificate cut down to its Ed25519 parts is not a
        // pgcert1, and a pgcert1 with the same key is signed by nobody the
        // server trusts.
        let c = hcert("alice", 1, 2);
        let line = c.encode();
        assert!(Cert::parse(line.as_bytes()).is_err());
    }

    /// The attack the typed lists stop: a quantum attacker who can find an
    /// Ed25519 secret for any point takes a listed hybrid id as an
    /// Ed25519 public key. Here the id is chosen to be an Ed25519 key
    /// whose seed is known, which is what such an attacker would have.
    #[test]
    fn a_hybrid_id_is_never_an_ed25519_key() {
        let attacker = [9u8; SEED];
        let point = crate::public_key(&attacker);
        let s = signed3();
        let offer = &s[..OFFER];
        let listed = [Id::Hybrid(point)];
        let p = make(&attacker, None, &s, "alice", "");
        assert_eq!(
            check(&typed(&listed, &[], false), offer, "alice", "", &p),
            Err(FAILED)
        );
        let v1 = {
            let mut v = crate::sign(&attacker, &CHAL).to_vec();
            v.extend_from_slice(&point);
            v
        };
        assert_eq!(
            check(&typed(&listed, &[], false), &CHAL, "alice", "", &v1),
            Err(FAILED)
        );
        // As a signer: a pgcert1 the attacker signs for its own key.
        let mut forged = Cert::new("alice", crate::public_key(&USER), 0).unwrap();
        forged.sign(&attacker).unwrap();
        let p = make(&USER, Some(&forged), &s, "alice", "");
        assert_eq!(
            check(&typed(&[], &listed, false), offer, "alice", "", &p),
            Err(UNTRUSTED)
        );
        // And the other way round: an Ed25519 signer listed bare does not
        // vouch for a pgcert2 whose signer's id has the same bytes.
        let c = hcert("alice", 1, 2);
        let raw = hybrid(2).public().id();
        let user = Secret::Hybrid(hybrid(1));
        let p = make_any(&user, Some(&c), &s, "alice", "");
        assert_eq!(
            check(
                &typed(&[], &[Id::Ed25519(raw)], false),
                offer,
                "alice",
                "",
                &p
            ),
            Err(UNTRUSTED)
        );
    }

    #[test]
    fn hybrid_certificates() {
        let c = hcert("alice", 1, 2);
        let signer = [Id::Hybrid(hybrid(2).public().id())];
        let user = Secret::Hybrid(hybrid(1));
        let s = signed3();
        let offer = &s[..OFFER];
        let p = make_any(&user, Some(&c), &s, "alice", "");
        assert_eq!(p[1], KIND_HYBRID_CERT);
        assert!(p.len() < MAX_LEN);
        assert_eq!(
            check(&typed(&[], &signer, false), offer, "alice", "", &p),
            Ok(())
        );
        assert_eq!(
            check(&typed(&[], &signer, true), offer, "alice", "", &p),
            Ok(())
        );
        assert_eq!(
            check(&typed(&[], &signer, false), offer, "bob", "", &p),
            Err(FAILED)
        );
        let p = make_any(&user, Some(&c), &s, "bob", "");
        assert_eq!(
            check(&typed(&[], &signer, false), offer, "bob", "", &p),
            Err(NAME_MISMATCH)
        );
        // Revoked by id.
        let mut pol = typed(&[], &signer, false);
        pol.revoked = Some(alloc::boxed::Box::leak(
            alloc::vec![signer[0]].into_boxed_slice(),
        ));
        let p = make_any(&user, Some(&c), &s, "alice", "");
        assert_eq!(check(&pol, offer, "alice", "", &p), Err(REVOKED));
        // An Ed25519 certificate where hybrid is required.
        let e = cert("alice", NOW + 60, &SIGNER);
        let pe = make(&USER, Some(&e), &s, "alice", "");
        let both = [signer[0], Id::Ed25519(crate::public_key(&SIGNER))];
        assert_eq!(
            check(&typed(&[], &both, false), offer, "alice", "", &pe),
            Ok(())
        );
        assert_eq!(
            check(&typed(&[], &both, true), offer, "alice", "", &pe),
            Err(PQSIG_REQUIRED)
        );
        // The largest certificate still fits a proof.
        let mut big = c.clone();
        for n in 3..3 + crate::cert::MAX_SIGS2 as u8 - 1 {
            big.sign(&Secret::Hybrid(hybrid(n))).unwrap();
        }
        let s4 = signed_pq(&[5u8; OFFER_PQ], &[9; 32], &[3; CT]);
        let p = make_any(&user, Some(&big), &s4, "alice", "");
        assert!(p.len() <= MAX_LEN, "{}", p.len());
        assert_eq!(
            check(
                &typed(&[], &signer, true),
                &[5u8; OFFER_PQ],
                "alice",
                "",
                &p
            ),
            Ok(())
        );
    }

    #[test]
    fn malformed() {
        let pol = policy(&[], &[], &[]);
        for p in [
            &[][..],
            &[2u8, 0][..],
            &[3u8; 200][..],
            &[2u8; 200][..],
            &[2u8; MAX_LEN + 1][..],
            &[4u8; 200][..],
            &[4u8; 2000][..],
        ] {
            assert_eq!(check(&pol, &CHAL, "a", "", p), Err(FAILED));
        }
    }
}
