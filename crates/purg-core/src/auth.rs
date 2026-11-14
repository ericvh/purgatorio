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
    CHALLENGE, ENCRYPTED, ENCRYPTION_REQUIRED, EXPIRED, FAILED, NAME_MISMATCH, NO_KEY, NO_PQ,
    NO_SESSION, NO_TRUST, NOT_REQUIRED, OFFER, OFFER_PQ, PQ_REQUIRED, PROOF, PUBLIC, REQUIRED,
    REVOKED, SEED, SERVER_FAILED, SERVER_NONE, SERVER_PROOF_OFFSET, SIGNATURE, SIGNED, SIGNED_PQ,
    UNTRUSTED,
};
pub use purg_auth::cert::Cert;
pub use purg_auth::{public_key, sign, verify};

use crate::pgrp::Pgrp;
use crate::sysfile::open;

/// `root/lib/test/auth.key` is this repeated byte. It is a public test
/// fixture, shared by the emu and the probe, not a secret to deploy.
pub const FIXTURE: u8 = 0x42;

/// `true` when `proof` is a version 1 proof by one of `allowed` over
/// `challenge`.
pub fn accept(allowed: &[[u8; PUBLIC]], challenge: &[u8], proof: &[u8]) -> bool {
    let policy = purg_auth::proof::Policy {
        keys: allowed,
        signers: &[],
        revoked: Some(&[]),
        now: 0,
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

/// Reads a 32-byte key file. Anything else, including a missing file, is
/// `None`.
pub(crate) fn read_key(pgrp: &Pgrp, path: &str) -> Option<[u8; PUBLIC]> {
    read_file(pgrp, path, PUBLIC)?.try_into().ok()
}

/// A name that can be a path element under `usr/`.
pub(crate) fn safe_user(user: &str) -> bool {
    !user.is_empty() && user != "." && user != ".." && !user.contains('/')
}

/// A key the client signs with: its seed (or `#¤` holding it) and, when it
/// has one, a certificate for its public key.
enum Signer {
    Seed([u8; SEED]),
    Factotum([u8; PUBLIC]),
}

/// Signs as `user` in `pgrp`'s namespace: finds the key, builds the
/// message from the proof's kind and body with `message`, and returns the
/// kind, the signature and the body. The key comes from `#¤` (with the
/// certificate loaded into it), an authinfo in
/// `/usr/<user>/keyring/default`, or a seed file: `/usr/<user>/key`, then
/// the public test key, which nothing trusts unless a test installs it.
fn sign_as(
    pgrp: &Pgrp,
    user: &str,
    message: impl FnOnce(u8, &[u8]) -> Vec<u8>,
) -> crate::error::Result<(u8, [u8; SIGNATURE], Vec<u8>)> {
    let (signer, cert) = client_key(pgrp, user)?;
    let public = match &signer {
        Signer::Seed(s) => public_key(s),
        Signer::Factotum(p) => *p,
    };
    let cert = cert.filter(|c| c.key == public);
    let (kind, body) = purg_auth::proof::body(cert.as_ref(), &public);
    let msg = message(kind, &body);
    let sig = match signer {
        Signer::Seed(s) => sign(&s, &msg),
        Signer::Factotum(_) => {
            let p = factotum_sign(pgrp, &msg).ok_or(crate::error::Error::new(NO_KEY))?;
            p[..SIGNATURE]
                .try_into()
                .map_err(|_| crate::error::Error::new(NO_KEY))?
        }
    };
    Ok((kind, sig, body))
}

/// Builds the client's proof for the kernel's own mount (`afd = -1`), with
/// the key [`sign_as`] finds. `challenge` is what the proof signs: the
/// [`SIGNED`] bytes of a version 3 exchange.
pub(crate) fn client_proof(
    pgrp: &Pgrp,
    user: &str,
    aname: &str,
    challenge: &[u8],
) -> crate::error::Result<Vec<u8>> {
    let (kind, sig, body) = sign_as(pgrp, user, |kind, body| {
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
    pgrp: &Pgrp,
    user: &str,
    keyfile: Option<&str>,
    keys: &purg_auth::session::Keys,
) -> Option<Vec<u8>> {
    if let Some(path) = keyfile {
        let text = read_file(pgrp, path, 4096)?;
        let info = purg_auth::keys::AuthInfo::parse(&text).ok()?;
        return Some(purg_auth::server::make(&info.seed, Some(&info.cert), keys));
    }
    if let Some(p) = factotum_server(pgrp, keys) {
        return Some(p);
    }
    let (kind, sig, body) = sign_as(pgrp, user, |kind, body| {
        purg_auth::server::message(keys, kind, body)
    })
    .ok()?;
    Some(purg_auth::server::assemble(kind, &sig, &body))
}

/// The guest factotum, mounted at this path in the exporter's namespace.
const FACTOTUM_RPC: &str = "/mnt/factotum/rpc";

/// Asks the guest factotum for a server proof over the session's
/// transcript and bound value (`docs/auth.md`, "Factotum"). `None` when
/// it is not mounted, has no key, or answers anything else.
fn factotum_server(pgrp: &Pgrp, keys: &purg_auth::session::Keys) -> Option<Vec<u8>> {
    let c = pgrp.namec(FACTOTUM_RPC, "/").ok()?;
    let c = open(c, purg_abi::mode::ORDWR).ok()?;
    let rpc = |req: &[u8]| -> Option<Vec<u8>> {
        if c.dev.write(&c, req, 0).ok()? != req.len() {
            return None;
        }
        let mut buf = alloc::vec![0u8; 4096];
        let n = c.dev.read(&c, &mut buf, 0).ok()?;
        buf.truncate(n);
        Some(buf)
    };
    // The key is the server's, whose certificate names the host rather
    // than the exporting user: no `user` attribute.
    if rpc(b"start proto=ed25519 role=server")? != b"ok" {
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
    pub keys: Vec<[u8; PUBLIC]>,
    pub signers: Vec<[u8; PUBLIC]>,
    pub revoked: Option<Vec<[u8; PUBLIC]>>,
}

impl Trust {
    /// Reads the signers and the revoked keys from `pgrp`'s namespace,
    /// with `keys` as the directly held keys. A missing revocation list
    /// revokes nothing; one that is there but cannot be read or parsed
    /// fails closed.
    fn read(pgrp: &Pgrp, keys: Vec<[u8; PUBLIC]>) -> Trust {
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
            && let Some(key) = read_key(pgrp, &alloc::format!("/usr/{uname}/key.pub"))
            && !keys.contains(&key)
        {
            keys.push(key);
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
        }
    }

    pub(crate) fn required(&self) -> bool {
        self.policy(0).requires_auth()
    }
}

/// Checks a server's proof for the session `keys` against what `pgrp`
/// trusts as a server ([`Trust::server`]).
pub(crate) fn check_server(
    pgrp: &Pgrp,
    now: u64,
    keys: &purg_auth::session::Keys,
    proof: &[u8],
) -> crate::error::Result<()> {
    let t = Trust::server(pgrp);
    purg_auth::server::check(&t.policy(now), keys, proof).map_err(crate::error::Error::new)
}

fn client_key(pgrp: &Pgrp, user: &str) -> crate::error::Result<(Signer, Option<Cert>)> {
    if let Some(ctl) = read_file(pgrp, "#¤/ctl", 4096)
        && let Some(hex) = ctl.strip_prefix(b"key ")
        && let Some(public) = hex
            .get(..PUBLIC * 2)
            .and_then(purg_auth::hex::array::<PUBLIC>)
    {
        let cert = purg_auth::keys::find_cert(&ctl);
        return Ok((Signer::Factotum(public), cert));
    }
    if safe_user(user) {
        let path = alloc::format!("/usr/{user}/keyring/default");
        if let Some(text) = read_file(pgrp, &path, 4096)
            && let Ok(info) = purg_auth::keys::AuthInfo::parse(&text)
        {
            return Ok((Signer::Seed(info.seed), Some(info.cert)));
        }
    }
    let mut paths = Vec::new();
    if safe_user(user) {
        paths.push(alloc::format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for path in paths {
        if let Some(key) = read_key(pgrp, &path) {
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
