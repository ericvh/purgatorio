//! The server's proof of a version 3 handshake (`docs/auth.md`, "Server
//! authentication").
//!
//! After the client's proof the server can prove itself in turn. Its proof
//! is
//!
//! ```text
//! server proof = kind u8 || signature[64] || body
//! message      = "pgauth3 server" 0x00 || transcript[32] || bind[32]
//!                                      || kind u8 || body
//! ```
//!
//! with `kind` and `body` as in the client's proof (a public key, or a
//! `pgcert1` line; for kinds 2 and 3 a hybrid key or a `pgcert2` line,
//! whose signature is a hybrid one of 3373 bytes instead of 64), and the
//! signature by the body's key over `message`.
//! `transcript` is SHA-256 of the client's signed message, which fixes the
//! challenge, both ephemeral keys, both names and the client's identity.
//! `bind` is a third output of the session's HKDF ([`Keys::bind`]), which
//! only the two ends of the exchange can compute. A proof is therefore
//! good for one session alone: one made for another session, or by a
//! relay that does not hold the session's secret, does not verify.
//!
//! On a mount it is read from the auth fid at offset 64 (after the offer),
//! through the record layer; on an rcmd connection it is the data of the
//! OK frame (`docs/rcmd.md`).

use alloc::vec::Vec;

use purg_abi::auth::{
    KIND_CERT, KIND_HYBRID_CERT, KIND_HYBRID_KEY, KIND_KEY, NAME_MISMATCH, PQSIG_REQUIRED,
    SERVER_FAILED, SERVER_NONE,
};

use crate::cert::{AnyCert, Cert};
use crate::proof::{MAX_LEN, Policy, check_cert, check_signed, signature_len};
use crate::session::Keys;
use crate::sig::{Public, Secret};
use crate::{PUBLIC, SEED};

/// The context string a server's signature starts with.
pub const CONTEXT: &[u8] = b"pgauth3 server\0";

/// The bytes a server's proof signs for the session `keys`.
pub fn message(keys: &Keys, kind: u8, body: &[u8]) -> Vec<u8> {
    message_for(&keys.transcript, &keys.bind, kind, body)
}

/// [`message`] from the session's transcript and bound value alone, as an
/// agent that holds the server's key but not the session's (factotum's
/// `role=server`) builds it.
pub fn message_for(transcript: &[u8; 32], bind: &[u8; 32], kind: u8, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTEXT.len() + 65 + body.len());
    m.extend_from_slice(CONTEXT);
    m.extend_from_slice(transcript);
    m.extend_from_slice(bind);
    m.push(kind);
    m.extend_from_slice(body);
    m
}

/// What an agent is given to make a server proof (factotum's
/// `role=server`): the session's transcript, then its bound value.
pub const SESSION: usize = 64;

/// A server proof from its parts.
pub fn assemble(kind: u8, signature: &[u8], body: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(1 + signature.len() + body.len());
    p.push(kind);
    p.extend_from_slice(signature);
    p.extend_from_slice(body);
    p
}

/// The server's proof for `keys`, signed with `seed` and carrying `cert`
/// when given.
pub fn make(seed: &[u8; SEED], cert: Option<&Cert>, keys: &Keys) -> Vec<u8> {
    let (kind, body) = crate::proof::body(cert, &crate::public_key(seed));
    let sig = crate::sign(seed, &message(keys, kind, &body));
    assemble(kind, &sig, &body)
}

/// [`make`] with a key of either kind.
pub fn make_any(secret: &Secret, cert: Option<&AnyCert>, keys: &Keys) -> Vec<u8> {
    let (kind, body) = crate::proof::body_any(cert, &secret.public());
    let sig = secret.sign(&message(keys, kind, &body));
    assemble(kind, &sig, &body)
}

/// A server proof taken apart.
pub struct Parsed<'a> {
    pub kind: u8,
    /// 64 bytes, or a hybrid signature for kinds 2 and 3.
    pub signature: &'a [u8],
    pub body: &'a [u8],
}

/// Splits a server proof. `None` when it is malformed.
pub fn parse(proof: &[u8]) -> Option<Parsed<'_>> {
    if proof.len() > MAX_LEN {
        return None;
    }
    let kind = *proof.first()?;
    let n = signature_len(kind)?;
    let signature = proof.get(1..1 + n)?;
    let body = proof.get(1 + n..)?;
    if body.is_empty() {
        return None;
    }
    Some(Parsed {
        kind,
        signature,
        body,
    })
}

/// The server's Ed25519 public key, once [`check`] has accepted its
/// proof. `None` for a hybrid one ([`key_any`]).
pub fn key(proof: &[u8]) -> Option<[u8; PUBLIC]> {
    let p = parse(proof)?;
    match p.kind {
        KIND_KEY => p.body.try_into().ok(),
        KIND_CERT => Cert::parse(p.body).ok().map(|c| c.key),
        _ => None,
    }
}

/// The server's public key of either kind.
pub fn key_any(proof: &[u8]) -> Option<Public> {
    let p = parse(proof)?;
    match p.kind {
        KIND_KEY | KIND_HYBRID_KEY => Public::from_slice(p.body),
        KIND_CERT | KIND_HYBRID_CERT => AnyCert::parse(p.body).ok().map(|c| c.key()),
        _ => None,
    }
}

/// The error a server proof fails with: [`PQSIG_REQUIRED`] says what was
/// wanted, anything else is [`SERVER_FAILED`].
fn failed(e: &'static str) -> &'static str {
    if e == PQSIG_REQUIRED {
        e
    } else {
        SERVER_FAILED
    }
}

/// Checks the server's `proof` for the session `keys` against what the
/// client trusts as a server: `policy.keys` are server keys it holds
/// directly, `policy.signers` the signers whose certificates it takes.
/// The certificate's name is not checked: any server a trusted signer
/// certified is accepted, as in Inferno.
///
/// An empty proof is [`SERVER_NONE`]; anything else that does not pass is
/// [`SERVER_FAILED`].
pub fn check(policy: &Policy<'_>, keys: &Keys, proof: &[u8]) -> Result<(), &'static str> {
    if proof.is_empty() {
        return Err(SERVER_NONE);
    }
    let p = parse(proof).ok_or(SERVER_FAILED)?;
    let msg = message(keys, p.kind, p.body);
    check_signed(policy, p.kind, p.signature, p.body, &msg, None)
        .map(|_| ())
        .map_err(failed)
}

/// Checks who the server in `proof` is, for a client that dialled
/// `host`: everything [`check_host`] checks except the signature over the
/// session, which only the holder of the session's keys can verify (the
/// kernel, for a mount; see `docs/auth.md`, "Servers per host").
///
/// `policy.keys` are the server keys trusted for `host` (the
/// `/lib/keys/servers` lines naming it or `*`), and `policy.signers` the
/// signers whose certificates are taken. A key proof must be one of those
/// keys. A certificate passes when its key is one of them, or when its
/// name is `host` (ASCII case ignored) and a trusted signer made it; one
/// for another name fails with [`NAME_MISMATCH`]. With no `host` (a mount
/// of a descriptor, which has no address) the name is not checked.
pub fn check_identity(
    policy: &Policy<'_>,
    proof: &[u8],
    host: Option<&str>,
) -> Result<(), &'static str> {
    if proof.is_empty() {
        return Err(SERVER_NONE);
    }
    let p = parse(proof).ok_or(SERVER_FAILED)?;
    let revoked = |k: &crate::sig::Id| policy.revoked.is_none_or(|r| r.contains(k));
    match p.kind {
        KIND_KEY | KIND_HYBRID_KEY => {
            let key = Public::from_slice(p.body).ok_or(SERVER_FAILED)?;
            if key.is_hybrid() != (p.kind == KIND_HYBRID_KEY) {
                return Err(SERVER_FAILED);
            }
            if policy.hybrid && !key.is_hybrid() {
                return Err(PQSIG_REQUIRED);
            }
            let id = key.id();
            if !policy.keys.contains(&id) || revoked(&id) {
                return Err(SERVER_FAILED);
            }
            Ok(())
        }
        KIND_CERT | KIND_HYBRID_CERT => {
            let cert = AnyCert::parse(p.body).map_err(|_| SERVER_FAILED)?;
            if cert.is_hybrid() != (p.kind == KIND_HYBRID_CERT) {
                return Err(SERVER_FAILED);
            }
            if let Some(h) = host
                && !policy.keys.contains(&cert.key().id())
                && !h.eq_ignore_ascii_case(cert.name())
            {
                return Err(NAME_MISMATCH);
            }
            check_cert(policy, &cert, None).map_err(failed)
        }
        _ => Err(SERVER_FAILED),
    }
}

/// [`check`] for a client that dialled `host`: the proof must verify for
/// the session `keys`, and [`check_identity`] must pass, so a server
/// certified for one host cannot pose as another.
pub fn check_host(
    policy: &Policy<'_>,
    keys: &Keys,
    proof: &[u8],
    host: Option<&str>,
) -> Result<(), &'static str> {
    check(policy, keys, proof)?;
    check_identity(policy, proof, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Client, Server};

    const USER: [u8; SEED] = [1; SEED];
    const HOST: [u8; SEED] = [3; SEED];
    const SIGNER: [u8; SEED] = [2; SEED];
    const NOW: u64 = 1_700_000_000;

    /// One exchange with fixed randomness: the server's and the client's
    /// view of its keys.
    fn exchange(challenge: u8) -> (Keys, Keys) {
        let server = Server::new([challenge; 32], [4; 32]);
        let client = Client::new(&server.offer, [5; 32]).unwrap();
        let proof = crate::proof::make(&USER, None, &client.signed, "alice", "");
        (
            server.keys("alice", "", &proof).unwrap(),
            client.keys("alice", "", &proof).unwrap(),
        )
    }

    use crate::SIGNATURE;
    use crate::proof::tests::ids;

    fn policy(keys: &[[u8; PUBLIC]], signers: &[[u8; PUBLIC]]) -> Policy<'static> {
        Policy {
            keys: ids(keys),
            signers: ids(signers),
            revoked: Some(&[]),
            now: NOW,
            hybrid: false,
        }
    }

    /// The derivation, against values computed with Python's `hmac` and
    /// `hashlib` (HKDF-SHA256 by hand), not with this code; then a proof
    /// over fixed keys, so the format cannot change unnoticed (Ed25519 is
    /// deterministic; that vector is this code's own).
    #[test]
    fn fixed_vectors() {
        let k = Keys::derive(&[1; 32], b"t");
        let hex = |b: &[u8; 32]| crate::hex::encode(b);
        assert_eq!(
            hex(&k.transcript),
            "e3b98a4da31a127d4bde6e43033f66ba274cab0eb7eb1c70ec41402bf6273dd8"
        );
        assert_eq!(
            hex(&k.c2s),
            "5e43a6e446004e9c301ed3cbc6118b39dfcbb1afeae723ed8d83fba215a0949a"
        );
        assert_eq!(
            hex(&k.s2c),
            "e20109d4ef89e30939b1f4ca3e22a52eb96f1354a439bcf8373ca10fffbb3918"
        );
        assert_eq!(
            hex(&k.bind),
            "745830caefd05b46f16021fe13b74e4fe23fe5e63f4e33aced78ab28af2c9590"
        );
        let m = message(&k, KIND_KEY, &[0xaa; 32]);
        assert_eq!(&m[..CONTEXT.len()], b"pgauth3 server\0");
        assert_eq!(m.len(), CONTEXT.len() + 32 + 32 + 1 + 32);
        assert_eq!(&m[CONTEXT.len()..CONTEXT.len() + 32], &k.transcript);
        assert_eq!(&m[CONTEXT.len() + 32..CONTEXT.len() + 64], &k.bind);
        let p = make(&HOST, None, &k);
        assert_eq!(p.len(), 1 + SIGNATURE + PUBLIC);
        assert_eq!(p[0], KIND_KEY);
        assert_eq!(&p[1 + SIGNATURE..], &crate::public_key(&HOST));
        assert_eq!(
            crate::hex::encode(&p[1..1 + SIGNATURE]),
            "f1b9765e46b14d79defe56e0d0b78b13178de83ec6e9c258c590f9adbab73f6d\
             1bdff3515f3e615daf493c3101cc6c513638fbf2a8d861c825e6f3ac84b0de09"
        );
    }

    #[test]
    fn a_proof_for_this_session_checks() {
        let (s, c) = exchange(9);
        let host = [crate::public_key(&HOST)];
        let p = make(&HOST, None, &s);
        assert_eq!(check(&policy(&host, &[]), &c, &p), Ok(()));
        assert_eq!(key(&p), Some(host[0]));
        // A key the client does not hold as a server's.
        assert_eq!(check(&policy(&[], &[]), &c, &p), Err(SERVER_FAILED));
        // Revoked.
        let pol = Policy {
            revoked: Some(ids(&host)),
            ..policy(&host, &[])
        };
        assert_eq!(check(&pol, &c, &p), Err(SERVER_FAILED));
        // No proof at all.
        assert_eq!(check(&policy(&host, &[]), &c, &[]), Err(SERVER_NONE));
        // Any changed byte fails.
        for i in 0..p.len() {
            let mut q = p.clone();
            q[i] ^= 1;
            assert!(check(&policy(&host, &[]), &c, &q).is_err(), "byte {i}");
        }
        for bad in [&[0u8][..], &[0u8; 65][..], &[7u8; 97][..]] {
            assert_eq!(check(&policy(&host, &[]), &c, bad), Err(SERVER_FAILED));
        }
    }

    /// A proof from another session does not check: neither the
    /// transcript nor the bound value match.
    #[test]
    fn a_proof_from_another_session_fails() {
        let (s1, _) = exchange(9);
        let (_, c2) = exchange(10);
        let host = [crate::public_key(&HOST)];
        let p = make(&HOST, None, &s1);
        assert_eq!(check(&policy(&host, &[]), &c2, &p), Err(SERVER_FAILED));
        // Same transcript, other secret: the bound value alone differs.
        let mut forged = c2.clone();
        forged.transcript = s1.transcript;
        assert_eq!(check(&policy(&host, &[]), &forged, &p), Err(SERVER_FAILED));
    }

    #[test]
    fn a_certified_server() {
        let (s, c) = exchange(9);
        let mut cert = Cert::new("host", crate::public_key(&HOST), NOW + 60).unwrap();
        cert.sign(&SIGNER).unwrap();
        let p = make(&HOST, Some(&cert), &s);
        assert_eq!(p[0], KIND_CERT);
        let signer = [crate::public_key(&SIGNER)];
        assert_eq!(check(&policy(&[], &signer), &c, &p), Ok(()));
        assert_eq!(check(&policy(&[], &[]), &c, &p), Err(SERVER_FAILED));
        let late = Policy {
            now: NOW + 61,
            ..policy(&[], &signer)
        };
        assert_eq!(check(&late, &c, &p), Err(SERVER_FAILED));
        // Signed by another key than the certificate's.
        let sig = crate::sign(&USER, &message(&c, KIND_CERT, cert.encode().as_bytes()));
        let q = assemble(KIND_CERT, &sig, cert.encode().as_bytes());
        assert_eq!(check(&policy(&[], &signer), &c, &q), Err(SERVER_FAILED));
    }

    /// A certificate from a trusted signer proves the host it names and no
    /// other: the impostor case of `docs/auth.md`, "Servers per host".
    #[test]
    fn a_server_is_checked_for_its_host() {
        let (s, c) = exchange(9);
        let mut cert = Cert::new("Box.example", crate::public_key(&HOST), 0).unwrap();
        cert.sign(&SIGNER).unwrap();
        let p = make(&HOST, Some(&cert), &s);
        let signer = [crate::public_key(&SIGNER)];
        let pol = policy(&[], &signer);
        assert_eq!(check_host(&pol, &c, &p, Some("box.example")), Ok(()));
        assert_eq!(check_host(&pol, &c, &p, Some("other")), Err(NAME_MISMATCH));
        assert_eq!(check_identity(&pol, &p, Some("other")), Err(NAME_MISMATCH));
        assert_eq!(check_identity(&pol, &p, Some("*")), Err(NAME_MISMATCH));
        assert_eq!(check_identity(&pol, &p, None), Ok(()));
        // A key listed for the host proves it whatever the name says.
        let host = [crate::public_key(&HOST)];
        assert_eq!(
            check_host(&policy(&host, &[]), &c, &p, Some("other")),
            Ok(())
        );
        // The right name from an untrusted signer is still refused.
        assert_eq!(
            check_identity(&policy(&[], &[]), &p, Some("box.example")),
            Err(SERVER_FAILED)
        );
        // Revoked keys, and a revocation list that did not parse.
        let revoked = Policy {
            revoked: Some(ids(&host)),
            ..policy(&[], &signer)
        };
        assert_eq!(
            check_identity(&revoked, &p, Some("box.example")),
            Err(SERVER_FAILED)
        );
        let broken = Policy {
            revoked: None,
            ..policy(&host, &[])
        };
        let q = make(&HOST, None, &s);
        assert_eq!(check_identity(&broken, &q, Some("h")), Err(SERVER_FAILED));
        // A bare key: only one listed for the host.
        assert_eq!(check_host(&policy(&host, &[]), &c, &q, Some("h")), Ok(()));
        assert_eq!(
            check_identity(&policy(&[], &signer), &q, Some("h")),
            Err(SERVER_FAILED)
        );
        // The identity check alone does not look at the signature: the
        // kernel does that, over the same bytes.
        let mut forged = p.clone();
        forged[1] ^= 1;
        assert_eq!(check_identity(&pol, &forged, Some("box.example")), Ok(()));
        assert_eq!(
            check_host(&pol, &c, &forged, Some("box.example")),
            Err(SERVER_FAILED)
        );
        assert_eq!(check_identity(&pol, &[], None), Err(SERVER_NONE));
    }

    #[test]
    fn message_for_matches_message() {
        let (s, _) = exchange(9);
        assert_eq!(
            message(&s, KIND_KEY, b"body"),
            message_for(&s.transcript, &s.bind, KIND_KEY, b"body")
        );
    }

    #[test]
    fn hybrid_server_proofs() {
        let (c, _) = exchange(1);
        let host = crate::sig::HybridSecret([3; crate::sig::HYBRID_SEED]);
        let secret = Secret::Hybrid(host.clone());
        let id = secret.public().id();
        let p = make_any(&secret, None, &c);
        assert_eq!(p[0], KIND_HYBRID_KEY);
        assert_eq!(key_any(&p), Some(secret.public()));
        assert_eq!(key(&p), None);
        let pol = |keys: &[crate::sig::Id], hybrid: bool| Policy {
            keys: alloc::boxed::Box::leak(keys.to_vec().into_boxed_slice()),
            signers: &[],
            revoked: Some(&[]),
            now: NOW,
            hybrid,
        };
        assert_eq!(check(&pol(&[id], true), &c, &p), Ok(()));
        assert_eq!(check_host(&pol(&[id], true), &c, &p, Some("h")), Ok(()));
        let crate::sig::Id::Hybrid(raw) = id else {
            panic!()
        };
        assert_eq!(
            check(&pol(&[crate::sig::Id::Ed25519(raw)], false), &c, &p),
            Err(SERVER_FAILED)
        );
        // Another session's keys.
        let (other, _) = exchange(2);
        assert_eq!(check(&pol(&[id], false), &other, &p), Err(SERVER_FAILED));
        // An Ed25519 server where hybrid is required.
        let e = make(&HOST, None, &c);
        let ed = crate::sig::Id::Ed25519(crate::public_key(&HOST));
        assert_eq!(check(&pol(&[ed], false), &c, &e), Ok(()));
        assert_eq!(check(&pol(&[ed], true), &c, &e), Err(PQSIG_REQUIRED));
        assert_eq!(
            check_identity(&pol(&[ed], true), &e, None),
            Err(PQSIG_REQUIRED)
        );
        // A certificate for the host.
        let signer = crate::sig::HybridSecret([4; crate::sig::HYBRID_SEED]);
        let mut cert = crate::cert::Cert2::new("box.example", host.public(), 0).unwrap();
        cert.sign(&signer).unwrap();
        let cert = AnyCert::V2(cert);
        let p = make_any(&secret, Some(&cert), &c);
        let sp = Policy {
            signers: alloc::boxed::Box::leak(
                alloc::vec![crate::sig::Id::Hybrid(signer.public().id())].into_boxed_slice(),
            ),
            ..pol(&[], true)
        };
        assert_eq!(check_host(&sp, &c, &p, Some("BOX.example")), Ok(()));
        assert_eq!(check_host(&sp, &c, &p, Some("other")), Err(NAME_MISMATCH));
    }
}
