//! Code shared by the authentication tools (`docs/auth.md`): keyfs
//! accounts, the server side of `pglogin1`, and option values.

use std::os::fd::FromRawFd;

pub mod synth;

use fmt::login::{self, Hello, Verifier};
use purgatorio::Error;
pub use purgatorio::auth::fmt;
pub use purgatorio::auth::{FileStream, Rng, now, read_line, user};

/// Where keyfs serves the key database.
pub const KEYS: &str = "/mnt/keys";
/// The signer's key.
pub const SIGNERKEY: &str = "/keydb/signerkey";
/// The key database file.
pub const KEYDB: &str = "/keydb/keys";
/// The trusted signers list.
pub const SIGNERS: &str = "/lib/keys/signers";
/// The revocation list.
pub const REVOKED: &str = "/lib/keys/revoked";
/// The keys trusted to sign modules (`docs/signing.md`).
pub const MODULES: &str = "/lib/keys/modules";

/// Parses `-a ed25519|hybrid`, the kind of key a tool makes: `true` for
/// a hybrid Ed25519 + ML-DSA-65 key (`docs/auth.md`, "Post-quantum
/// signatures").
pub fn alg(s: &str) -> Result<bool, Error> {
    match s {
        "ed25519" => Ok(false),
        "hybrid" => Ok(true),
        _ => Err(Error(format!(
            "unknown algorithm {s}: want ed25519 or hybrid"
        ))),
    }
}

/// A fresh key of the kind [`alg`] named.
pub fn new_key(hybrid: bool) -> fmt::sig::Secret {
    if hybrid {
        let mut s = [0u8; fmt::sig::HYBRID_SEED];
        Rng::fill(&mut s);
        fmt::sig::Secret::Hybrid(fmt::sig::HybridSecret(s))
    } else {
        fmt::sig::Secret::Ed25519(Rng::seed())
    }
}

/// The name of a key's kind, as `-a` takes it.
pub fn kind(hybrid: bool) -> &'static str {
    if hybrid { "hybrid" } else { "ed25519" }
}

/// Parses `-C m,t`: Argon2id memory in KiB and passes.
pub fn cost(s: &str) -> Result<(u32, u32), Error> {
    let bad = || Error(format!("bad cost {s}: want m,t"));
    let (m, t) = s.split_once(',').ok_or_else(bad)?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    let t: u32 = t.parse().map_err(|_| bad())?;
    if m < 8 || t < 1 || m > login::MAX_M || t > login::MAX_T {
        return Err(bad());
    }
    Ok((m, t))
}

/// Parses an expiry: `never` (or `0`), or seconds since 1970.
pub fn expiry(s: &str) -> Result<u64, Error> {
    if s == "never" {
        return Ok(0);
    }
    fmt::cert::parse_u64(s).ok_or_else(|| Error(format!("bad expiry {s}")))
}

/// The text form of an expiry.
pub fn show_expiry(e: u64) -> String {
    if e == 0 {
        String::from("never")
    } else {
        e.to_string()
    }
}

/// A keyfs account.
pub struct Account {
    pub verifier: Option<Verifier>,
    /// 0 when it does not expire.
    pub expire: u64,
    pub disabled: bool,
}

impl Account {
    pub fn expired(&self, now: u64) -> bool {
        self.expire != 0 && now >= self.expire
    }
}

/// Reads `<mnt>/<name>`. `None` when there is no such user.
pub fn account(mnt: &str, name: &str) -> Option<Account> {
    if !fmt::cert::valid_name(name) {
        return None;
    }
    let dir = format!("{mnt}/{name}");
    let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}"));
    let status = read("status").ok()?;
    let verifier = read("secret")
        .ok()
        .and_then(|s| Verifier::parse(s.as_bytes()).ok());
    let expire = read("expire")
        .ok()
        .and_then(|s| expiry(s.trim()).ok())
        .unwrap_or(0);
    Some(Account {
        verifier,
        expire,
        disabled: status.trim() != "ok",
    })
}

/// A secret for [`Verifier::fake`]: the signer's key when it can be read.
pub fn fake_secret(keyfile: &str) -> Vec<u8> {
    std::fs::read(keyfile).unwrap_or_else(|_| b"pglogin1".to_vec())
}

/// Serves one `pglogin1` conversation on fd 0 for the operations in `ops`.
/// `handler` gets the client's hello, its account and the opened request,
/// and returns the reply to seal. Errors after the password check are sent
/// sealed, so only the password's holder sees them.
pub fn serve_login(
    ops: &[&str],
    mnt: &str,
    fake: &[u8],
    handler: impl FnOnce(&Hello, &Account, &[u8]) -> Result<Vec<u8>, String>,
) -> Result<(), Error> {
    // SAFETY: fd 0 is the connection, owned by this prog from here on.
    let mut f = unsafe { std::fs::File::from_raw_fd(0) };
    let mut s = FileStream(&mut f);
    let hello = login::server_hello(&mut s).map_err(Error)?;
    if !ops.contains(&hello.op.as_str()) {
        let _ = login::send_error(&mut s, "unknown request");
        return Err(Error(format!("unknown request {}", hello.op)));
    }
    let acct = account(mnt, &hello.name);
    let v = acct
        .as_ref()
        .and_then(|a| a.verifier)
        .unwrap_or_else(|| Verifier::fake(fake, &hello.name, &mut Rng));
    let (mut sess, req) = login::server(&mut s, &hello, &v, &mut Rng).map_err(Error)?;
    let Some(acct) = acct else {
        return Err(Error::from(login::FAILED));
    };
    let reply = if acct.disabled {
        Err(String::from("account disabled"))
    } else if acct.expired(now()) {
        Err(String::from("account expired"))
    } else {
        handler(&hello, &acct, &req)
    };
    match reply {
        Ok(r) => sess.send(&mut s, &r).map_err(Error),
        Err(e) => {
            let _ = sess.send(&mut s, format!("error {e}").as_bytes());
            Err(Error(format!("{}: {e}", hello.name)))
        }
    }
}

/// The client side: dials `addr` and runs `op` as `name`. A reply of
/// `error <reason>` becomes that error; otherwise the reply after `ok`.
pub fn login(
    addr: &str,
    op: &str,
    name: &str,
    password: &str,
    request: impl FnOnce(&login::Kdf) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, Error> {
    let conn = purgatorio::dial::dial(addr)?;
    let data = conn
        .data
        .ok_or_else(|| Error(format!("{addr}: no connection")))?;
    let mut f = std::fs::File::from(data);
    let mut s = FileStream(&mut f);
    let (_, reply) =
        login::client(&mut s, op, name, password.as_bytes(), &mut Rng, request).map_err(Error)?;
    if let Some(e) = reply.strip_prefix(b"error ") {
        return Err(Error(String::from_utf8_lossy(e).into_owned()));
    }
    match reply.strip_prefix(b"ok") {
        Some(rest) => Ok(rest.strip_prefix(b"\n").unwrap_or(rest).to_vec()),
        None => Err(Error::from(login::EPROTO)),
    }
}

/// Reads a password line from standard input.
pub fn password(what: &str) -> Result<String, Error> {
    read_line().ok_or_else(|| Error(format!("no {what}")))
}

/// Makes the directories leading to `path`, as `mkdir -p` would.
pub fn mkdirs_for(path: &str) -> Result<(), Error> {
    let Some((dir, _)) = path.rsplit_once('/') else {
        return Ok(());
    };
    let mut cur = String::new();
    for part in dir.split('/') {
        if part.is_empty() {
            if cur.is_empty() {
                cur.push('/');
            }
            continue;
        }
        if !cur.ends_with('/') {
            cur.push('/');
        }
        cur.push_str(part);
        if purgatorio::stat(&cur).is_err() {
            purgatorio::create(
                &cur,
                purgatorio::abi::mode::OREAD,
                purgatorio::DMDIR | 0o700,
            )
            .map_err(|e| Error(format!("{cur}: {}", e)))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        assert_eq!(cost("8,1").unwrap(), (8, 1));
        assert!(cost("7,1").is_err());
        assert!(cost("8").is_err());
        assert_eq!(expiry("never").unwrap(), 0);
        assert_eq!(expiry("12").unwrap(), 12);
        assert!(expiry("-1").is_err());
        assert_eq!(show_expiry(0), "never");
    }
}
