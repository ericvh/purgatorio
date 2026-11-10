//! Mount proofs. The client writes one in a single write to the auth fid:
//!
//! - version 1, exactly [`V1_LEN`] bytes: `signature || public_key` over
//!   the raw challenge;
//! - version 2: `0x02 || kind || signature[64] || body`, where the body is
//!   a public key (kind 0) or a `pgcert1` line (kind 1), and the signature
//!   is over [`message`].

use alloc::vec::Vec;

use purg_abi::auth::{
    EXPIRED, FAILED, KIND_CERT, KIND_KEY, NAME_MISMATCH, PROOF, PROOF_VERSION, REVOKED, UNTRUSTED,
};

use crate::cert::Cert;
use crate::{PUBLIC, SEED, SIGNATURE};

/// The length of a version 1 proof.
pub const V1_LEN: usize = PROOF;
/// The context string a version 2 signature starts with.
pub const CONTEXT: &[u8] = b"pgauth2\0";
/// A proof is at most this long (a certificate with every signature fits).
pub const MAX_LEN: usize = 2048;

/// The bytes a version 2 proof signs.
pub fn message(challenge: &[u8], uname: &str, aname: &str, kind: u8, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTEXT.len() + challenge.len() + 5 + body.len() + 64);
    m.extend_from_slice(CONTEXT);
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

/// Assembles a version 2 proof from its parts.
pub fn assemble(kind: u8, signature: &[u8; SIGNATURE], body: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(2 + SIGNATURE + body.len());
    p.push(PROOF_VERSION);
    p.push(kind);
    p.extend_from_slice(signature);
    p.extend_from_slice(body);
    p
}

/// A version 2 proof signed with `seed`, carrying `cert` when given.
pub fn make(
    seed: &[u8; SEED],
    cert: Option<&Cert>,
    challenge: &[u8],
    uname: &str,
    aname: &str,
) -> Vec<u8> {
    let (kind, body) = body(cert, &crate::public_key(seed));
    let sig = crate::sign(seed, &message(challenge, uname, aname, kind, &body));
    assemble(kind, &sig, &body)
}

/// What a server trusts.
pub struct Policy<'a> {
    /// Keys the server holds directly.
    pub keys: &'a [[u8; PUBLIC]],
    /// Trusted signers.
    pub signers: &'a [[u8; PUBLIC]],
    /// Revoked keys. `None` when the list did not parse: every
    /// certificate then fails.
    pub revoked: Option<&'a [[u8; PUBLIC]]>,
    /// Seconds since 1970 UTC.
    pub now: u64,
}

impl Policy<'_> {
    /// Whether this policy can accept anything at all.
    pub fn requires_auth(&self) -> bool {
        !self.keys.is_empty() || !self.signers.is_empty()
    }

    fn revoked(&self, key: &[u8; PUBLIC]) -> bool {
        self.revoked.is_some_and(|r| r.contains(key))
    }
}

/// Checks `proof` against `policy` for this `Tauth`. Returns the error the
/// write fails with.
pub fn check(
    policy: &Policy<'_>,
    challenge: &[u8],
    uname: &str,
    aname: &str,
    proof: &[u8],
) -> Result<(), &'static str> {
    if proof.len() == V1_LEN {
        let (sig, key) = proof.split_at(SIGNATURE);
        let sig: [u8; SIGNATURE] = sig.try_into().map_err(|_| FAILED)?;
        let key: [u8; PUBLIC] = key.try_into().map_err(|_| FAILED)?;
        if !policy.keys.contains(&key) || !crate::verify(&key, challenge, &sig) {
            return Err(FAILED);
        }
        return if policy.revoked(&key) {
            Err(REVOKED)
        } else {
            Ok(())
        };
    }
    if proof.len() < 2 + SIGNATURE || proof.len() > MAX_LEN || proof[0] != PROOF_VERSION {
        return Err(FAILED);
    }
    let kind = proof[1];
    let sig: [u8; SIGNATURE] = proof[2..2 + SIGNATURE].try_into().map_err(|_| FAILED)?;
    let body = &proof[2 + SIGNATURE..];
    let signed = message(challenge, uname, aname, kind, body);
    match kind {
        KIND_KEY => {
            let key: [u8; PUBLIC] = body.try_into().map_err(|_| FAILED)?;
            if !policy.keys.contains(&key) || !crate::verify(&key, &signed, &sig) {
                return Err(FAILED);
            }
            if policy.revoked(&key) {
                return Err(REVOKED);
            }
            Ok(())
        }
        KIND_CERT => {
            let cert = Cert::parse(body).map_err(|_| FAILED)?;
            if !crate::verify(&cert.key, &signed, &sig) {
                return Err(FAILED);
            }
            check_cert(policy, &cert, Some(uname))
        }
        _ => Err(FAILED),
    }
}

/// Checks a certificate against `policy`, as a server does once the
/// proof's signature verifies. `uname`, when given, must be its name.
pub fn check_cert(
    policy: &Policy<'_>,
    cert: &Cert,
    uname: Option<&str>,
) -> Result<(), &'static str> {
    if policy.revoked.is_none() || policy.revoked(&cert.key) {
        return Err(REVOKED);
    }
    if policy.keys.contains(&cert.key) {
        return Ok(());
    }
    if uname.is_some_and(|u| u != cert.name) {
        return Err(NAME_MISMATCH);
    }
    if cert.expired(policy.now) {
        return Err(EXPIRED);
    }
    let trusted = |s: &[u8; PUBLIC]| policy.signers.contains(s) && cert.signed_by(s);
    if cert
        .sigs
        .iter()
        .any(|(s, _)| trusted(s) && !policy.revoked(s))
    {
        return Ok(());
    }
    if cert.sigs.iter().any(|(s, _)| trusted(s)) {
        return Err(REVOKED);
    }
    Err(UNTRUSTED)
}

#[cfg(test)]
mod tests {
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

    fn policy<'a>(
        keys: &'a [[u8; PUBLIC]],
        signers: &'a [[u8; PUBLIC]],
        revoked: &'a [[u8; PUBLIC]],
    ) -> Policy<'a> {
        Policy {
            keys,
            signers,
            revoked: Some(revoked),
            now: NOW,
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
            signers: &signer,
            revoked: None,
            now: NOW,
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

    #[test]
    fn malformed() {
        let pol = policy(&[], &[], &[]);
        for p in [
            &[][..],
            &[2u8, 0][..],
            &[3u8; 200][..],
            &[2u8; 200][..],
            &[2u8; MAX_LEN + 1][..],
        ] {
            assert_eq!(check(&pol, &CHAL, "a", "", p), Err(FAILED));
        }
    }
}
