//! Purgatorio mount authentication (`docs/auth.md`). This is not Inferno's
//! keyring protocol. Real Inferno clients still mount with `-A`.
//!
//! The client writes one proof to the auth fid: version 1, `signature[64]
//! || public_key[32]` over the raw challenge, version 2, which signs a
//! domain-separated message and can carry a signer's certificate, or
//! version 3, which also agrees a session key that encrypts the rest of
//! the conversation. The formats and the server's check are in
//! `purg_auth`.

use alloc::string::String;
use alloc::vec::Vec;

pub use purg_abi::auth::{
    CHALLENGE, ENCRYPTED, ENCRYPTION_REQUIRED, EXPIRED, FAILED, HYBRID_PUBLIC, NAME_MISMATCH,
    NO_KEY, NO_PQ, NO_SESSION, NO_TRUST, NOT_REQUIRED, OFFER, OFFER_PQ, PQ_REQUIRED,
    PQSIG_REQUIRED, PROOF, PUBLIC, REQUIRED, REVOKED, SEED, SERVER_FAILED, SERVER_NONE,
    SERVER_PROOF_OFFSET, SIGNATURE, SIGNED, SIGNED_PQ, UNTRUSTED,
};
pub use purg_auth::cert::{AnyCert, Cert};
pub use purg_auth::sig::{Id, Public, Secret};
pub use purg_auth::{public_key, sign, verify};

use crate::pgrp::Pgrp;
use crate::sysfile::open;

/// `root/lib/test/auth.key` is this repeated byte. It is a public test
/// fixture, shared by the emu and the probe, not a secret to deploy.
pub const FIXTURE: u8 = 0x42;

/// `true` when `proof` is a version 1 proof by one of `allowed` over
/// `challenge`.
pub fn accept(allowed: &[[u8; PUBLIC]], challenge: &[u8], proof: &[u8]) -> bool {
    let keys: Vec<Id> = allowed.iter().map(Id::from).collect();
    let policy = purg_auth::proof::Policy {
        keys: &keys,
        signers: &[],
        revoked: Some(&[]),
        now: 0,
        hybrid: false,
    };
    proof.len() == PROOF && purg_auth::proof::check(&policy, challenge, "", "", proof).is_ok()
}

/// Reads a whole small file (at most `max` bytes). `None` when it is
/// missing, unreadable or larger.
pub(crate) fn read_file(pgrp: &Pgrp, path: &str, max: usize) -> Option<Vec<u8>> {
    let c = pgrp.namec(path, "/").ok()?;
    let c = open(c, purg_abi::mode::OREAD).ok()?;
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = c.dev.read(&c, &mut buf, out.len() as u64).ok()?;
        if n == 0 {
            return Some(out);
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > max {
            return None;
        }
    }
}

/// Reads a public key file: 32 bytes for Ed25519, [`HYBRID_PUBLIC`] for
/// a hybrid key. Anything else, including a missing file, is `None`.
pub(crate) fn read_public(pgrp: &Pgrp, path: &str) -> Option<Public> {
    Public::from_slice(&read_file(pgrp, path, HYBRID_PUBLIC)?)
}

/// Reads a seed file: 32 bytes for Ed25519, 64 for a hybrid key.
pub(crate) fn read_seed(pgrp: &Pgrp, path: &str) -> Option<Secret> {
    Secret::from_seed_file(&read_file(pgrp, path, purg_auth::sig::HYBRID_SEED)?)
}

/// The stack ML-DSA work gets ([`deep`]): it takes about 190 KiB at its
/// deepest natively, and a kernel thread has 256 KiB, an export worker 64.
const DEEP_STACK: usize = 512 << 10;

/// Runs `f`, which signs or verifies with a hybrid key when `hybrid`, on
/// a stack large enough for ML-DSA (`Platform::deep`). Ed25519 work runs
/// in place.
pub(crate) fn deep<T: Send>(
    platform: &dyn purg_platform::Platform,
    hybrid: bool,
    f: impl FnOnce() -> T + Send,
) -> T {
    if !hybrid {
        return f();
    }
    let mut f = Some(f);
    let mut out = None;
    platform.deep(DEEP_STACK, &mut || {
        if let Some(f) = f.take() {
            out = Some(f());
        }
    });
    match out {
        Some(v) => v,
        // `deep` returns only once `f` has run.
        None => unreachable!("Platform::deep did not run its closure"),
    }
}

/// Whether a proof (or a server's proof, from `at`, where its kind is)
/// is by a hybrid key, so checking it needs [`deep`].
pub(crate) fn hybrid_kind(proof: &[u8], at: usize) -> bool {
    proof.get(at).is_some_and(|&k| {
        k == purg_abi::auth::KIND_HYBRID_KEY || k == purg_abi::auth::KIND_HYBRID_CERT
    })
}

/// The longest authinfo read: a hybrid one with a `pgcert2` that has
/// every signature.
const MAX_INFO: usize = 64 * 1024;

/// A name that can be a path element under `usr/`.
pub(crate) fn safe_user(user: &str) -> bool {
    !user.is_empty() && user != "." && user != ".." && !user.contains('/')
}

/// A key the client signs with: its seed (or `#¤` holding it) and, when it
/// has one, a certificate for its public key.
enum Signer {
    Seed(Secret),
    Factotum([u8; PUBLIC]),
}

/// Signs as `user` in `pgrp`'s namespace: finds the key, builds the
/// message from the proof's kind and body with `message`, and returns the
/// kind, the signature and the body. The key comes from `#¤` (with the
/// certificate loaded into it), an authinfo in
/// `/usr/<user>/keyring/default`, or a seed file: `/usr/<user>/key`, then
/// the public test key, which nothing trusts unless a test installs it.
fn sign_as(
    platform: &dyn purg_platform::Platform,
    pgrp: &Pgrp,
    user: &str,
    message: impl FnOnce(u8, &[u8]) -> Vec<u8>,
) -> crate::error::Result<(u8, Vec<u8>, Vec<u8>)> {
    let (signer, cert) = client_key(pgrp, user)?;
    let public = match &signer {
        // A hybrid key's public half takes an ML-DSA key generation; the
        // certificate holds it already.
        Signer::Seed(s @ Secret::Hybrid(_)) => match cert.as_ref().map(AnyCert::key) {
            Some(p @ Public::Hybrid(_)) => p,
            _ => deep(platform, true, || s.public()),
        },
        Signer::Seed(s) => s.public(),
        Signer::Factotum(p) => Public::Ed25519(*p),
    };
    let cert = cert.filter(|c| c.key() == public);
    let (kind, body) = purg_auth::proof::body_any(cert.as_ref(), &public);
    let msg = message(kind, &body);
    let sig = match signer {
        Signer::Seed(s) => deep(platform, s.is_hybrid(), || s.sign(&msg)),
        Signer::Factotum(_) => {
            let p = factotum_sign(pgrp, &msg).ok_or(crate::error::Error::new(NO_KEY))?;
            p[..SIGNATURE].to_vec()
        }
    };
    Ok((kind, sig, body))
}

/// Builds the client's proof for the kernel's own mount (`afd = -1`), with
/// the key [`sign_as`] finds. `challenge` is what the proof signs: the
/// [`SIGNED`] bytes of a version 3 exchange.
pub(crate) fn client_proof(
    platform: &dyn purg_platform::Platform,
    pgrp: &Pgrp,
    user: &str,
    aname: &str,
    challenge: &[u8],
) -> crate::error::Result<Vec<u8>> {
    let (kind, sig, body) = sign_as(platform, pgrp, user, |kind, body| {
        purg_auth::proof::message(challenge, user, aname, kind, body)
    })?;
    Ok(purg_auth::proof::assemble(challenge, kind, &sig, &body))
}

/// The server's proof for a version 3 session (`purg_auth::server`), made
/// as `user` in `pgrp`'s namespace. The key is the authinfo `keyfile` when
/// given; else the guest factotum's (`/mnt/factotum/rpc`, `role=server`),
/// so the exporting prog need not be able to read the key; else the key a
/// mount by `user` would prove with. `None` when there is no key.
pub(crate) fn server_proof(
    platform: &dyn purg_platform::Platform,
    pgrp: &Pgrp,
    user: &str,
    keyfile: Option<&str>,
    keys: &purg_auth::session::Keys,
    hybrid: bool,
) -> Option<Vec<u8>> {
    if let Some(path) = keyfile {
        let text = read_file(pgrp, path, MAX_INFO)?;
        let info = purg_auth::keys::AuthInfo::parse(&text).ok()?;
        return Some(deep(platform, info.key.is_hybrid(), || {
            purg_auth::server::make_any(&info.key, Some(&info.cert), keys)
        }));
    }
    if let Some(p) = factotum_server(pgrp, keys, hybrid) {
        return Some(p);
    }
    let (kind, sig, body) = sign_as(platform, pgrp, user, |kind, body| {
        purg_auth::server::message(keys, kind, body)
    })
    .ok()?;
    Some(purg_auth::server::assemble(kind, &sig, &body))
}

/// The guest factotum, mounted at this path in the exporter's namespace.
const FACTOTUM_RPC: &str = "/mnt/factotum/rpc";

/// Asks the guest factotum for a server proof over the session's
/// transcript and bound value (`docs/auth.md`, "Factotum"): with its
/// hybrid key first when the client proved with one (`hybrid`), else
/// with its Ed25519 key first. `None` when it is not mounted, has no key,
/// or answers anything else.
fn factotum_server(pgrp: &Pgrp, keys: &purg_auth::session::Keys, hybrid: bool) -> Option<Vec<u8>> {
    let order = if hybrid {
        ["hybrid", "ed25519"]
    } else {
        ["ed25519", "hybrid"]
    };
    order
        .iter()
        .find_map(|p| factotum_server_proto(pgrp, keys, p))
}

fn factotum_server_proto(
    pgrp: &Pgrp,
    keys: &purg_auth::session::Keys,
    proto: &str,
) -> Option<Vec<u8>> {
    let c = pgrp.namec(FACTOTUM_RPC, "/").ok()?;
    let c = open(c, purg_abi::mode::ORDWR).ok()?;
    let rpc = |req: &[u8]| -> Option<Vec<u8>> {
        if c.dev.write(&c, req, 0).ok()? != req.len() {
            return None;
        }
        let mut buf = alloc::vec![0u8; purg_auth::proof::MAX_LEN + 3];
        let n = c.dev.read(&c, &mut buf, 0).ok()?;
        buf.truncate(n);
        Some(buf)
    };
    // The key is the server's, whose certificate names the host rather
    // than the exporting user: no `user` attribute.
    let start = alloc::format!("start proto={proto} role=server");
    if rpc(start.as_bytes())? != b"ok" {
        return None;
    }
    let mut w = b"write ".to_vec();
    w.extend_from_slice(&keys.transcript);
    w.extend_from_slice(&keys.bind);
    if rpc(&w)? != b"ok" {
        return None;
    }
    let proof = rpc(b"read")?.strip_prefix(b"ok ")?.to_vec();
    purg_auth::server::parse(&proof)?;
    Some(proof)
}

/// The longest key list read.
const MAX_LIST: usize = 64 * 1024;
/// Trusted signers and revoked keys (key lists), and the keys held
/// directly for users and for servers (named key lists).
const SIGNERS_LIST: &str = "/lib/keys/signers";
const REVOKED_LIST: &str = "/lib/keys/revoked";
const USERS: &str = "/lib/keys/users";
const SERVERS: &str = "/lib/keys/servers";
/// Signers trusted for servers only. When it is there, it replaces
/// `/lib/keys/signers` for server proofs (`docs/auth.md`, "Signers for
/// servers").
const SERVER_SIGNERS: &str = "/lib/keys/serversigners";

/// What one side trusts: keys it holds directly, signers, and revoked keys
/// (`None` when that list does not parse).
pub(crate) struct Trust {
    pub keys: Vec<Id>,
    pub signers: Vec<Id>,
    pub revoked: Option<Vec<Id>>,
    /// Take only hybrid keys and certificates (`MPQSIG`).
    pub hybrid: bool,
}

impl Trust {
    /// Reads the signers and the revoked keys from `pgrp`'s namespace,
    /// with `keys` as the directly held keys. A missing revocation list
    /// revokes nothing; one that is there but cannot be read or parsed
    /// fails closed.
    fn read(pgrp: &Pgrp, keys: Vec<Id>) -> Trust {
        let signers = read_file(pgrp, SIGNERS_LIST, MAX_LIST)
            .map(|t| purg_auth::keys::parse_list_lenient(&t))
            .unwrap_or_default();
        let revoked = match read_file(pgrp, REVOKED_LIST, MAX_LIST) {
            Some(t) => purg_auth::keys::parse_list(&t),
            None if pgrp.namec(REVOKED_LIST, "/").is_ok() => None,
            None => Some(Vec::new()),
        };
        Trust {
            keys,
            signers,
            revoked,
            hybrid: false,
        }
    }

    /// What a server in `pgrp` trusts for `uname`: the keys
    /// `/lib/keys/users` lists for `uname` or `*`, `/usr/<uname>/key.pub`,
    /// `/lib/keys/signers` and `/lib/keys/revoked`. Nothing else: the
    /// stock root trusts no key (`docs/auth.md`, "Trust by default").
    pub(crate) fn client(pgrp: &Pgrp, uname: &str) -> Trust {
        let users = read_file(pgrp, USERS, MAX_LIST)
            .map(|t| purg_auth::keys::parse_named(&t))
            .unwrap_or_default();
        let mut keys = purg_auth::keys::keys_for(&users, Some(uname));
        if safe_user(uname)
            && let Some(key) = read_public(pgrp, &alloc::format!("/usr/{uname}/key.pub"))
            && !keys.contains(&key.id())
        {
            keys.push(key.id());
        }
        Trust::read(pgrp, keys)
    }

    /// What a client in `pgrp` trusts as a server, whatever its host: every
    /// key in `/lib/keys/servers`, and certificates from
    /// `/lib/keys/serversigners`, or from `/lib/keys/signers` when there
    /// is no such file, less `/lib/keys/revoked`. A `serversigners` that
    /// is there but cannot be read trusts no signer. The host a proof is
    /// for is the guest's to check (`docs/auth.md`, "Servers per host").
    pub(crate) fn server(pgrp: &Pgrp) -> Trust {
        let servers = read_file(pgrp, SERVERS, MAX_LIST)
            .map(|t| purg_auth::keys::parse_named(&t))
            .unwrap_or_default();
        let mut t = Trust::read(pgrp, purg_auth::keys::keys_for(&servers, None));
        if let Some(text) = read_file(pgrp, SERVER_SIGNERS, MAX_LIST) {
            t.signers = purg_auth::keys::parse_list_lenient(&text);
        } else if pgrp.namec(SERVER_SIGNERS, "/").is_ok() {
            t.signers = Vec::new();
        }
        t
    }

    pub(crate) fn policy(&self, now: u64) -> purg_auth::proof::Policy<'_> {
        purg_auth::proof::Policy {
            keys: &self.keys,
            signers: &self.signers,
            revoked: self.revoked.as_deref(),
            now,
            hybrid: self.hybrid,
        }
    }

    pub(crate) fn required(&self) -> bool {
        self.policy(0).requires_auth()
    }
}

/// Checks a server's proof for the session `keys` against what `pgrp`
/// trusts as a server ([`Trust::server`]).
pub(crate) fn check_server(
    platform: &dyn purg_platform::Platform,
    pgrp: &Pgrp,
    now: u64,
    hybrid: bool,
    keys: &purg_auth::session::Keys,
    proof: &[u8],
) -> crate::error::Result<()> {
    let mut t = Trust::server(pgrp);
    t.hybrid = hybrid;
    let policy = t.policy(now);
    deep(platform, hybrid_kind(proof, 0), || {
        purg_auth::server::check(&policy, keys, proof)
    })
    .map_err(crate::error::Error::new)
}

fn client_key(pgrp: &Pgrp, user: &str) -> crate::error::Result<(Signer, Option<AnyCert>)> {
    if let Some(ctl) = read_file(pgrp, "#¤/ctl", 4096)
        && let Some(hex) = ctl.strip_prefix(b"key ")
        && let Some(public) = hex
            .get(..PUBLIC * 2)
            .and_then(purg_auth::hex::array::<PUBLIC>)
    {
        let cert = purg_auth::keys::find_cert(&ctl).map(AnyCert::V1);
        return Ok((Signer::Factotum(public), cert));
    }
    if safe_user(user) {
        let path = alloc::format!("/usr/{user}/keyring/default");
        if let Some(text) = read_file(pgrp, &path, MAX_INFO)
            && let Ok(info) = purg_auth::keys::AuthInfo::parse(&text)
        {
            return Ok((Signer::Seed(info.key), Some(info.cert)));
        }
    }
    let mut paths = Vec::new();
    if safe_user(user) {
        paths.push(alloc::format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for path in paths {
        if let Some(key) = read_seed(pgrp, &path) {
            return Ok((Signer::Seed(key), None));
        }
    }
    Err(crate::error::Error::new(NO_KEY))
}

/// Asks `#¤/rpc` to sign `msg`. `None` when the agent has no key.
fn factotum_sign(pgrp: &Pgrp, msg: &[u8]) -> Option<[u8; PROOF]> {
    let c = pgrp.namec("#¤/rpc", "/").ok()?;
    let c = open(c, purg_abi::mode::ORDWR).ok()?;
    if c.dev.write(&c, msg, 0).ok()? != msg.len() {
        return None;
    }
    let mut buf = [0u8; PROOF];
    let n = c.dev.read(&c, &mut buf, 0).ok()?;
    (n == buf.len()).then_some(buf)
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
