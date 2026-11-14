//! Key lists and authinfo files.
//!
//! A key list has one key per line: an Ed25519 public key as 64 hex
//! digits, or a hybrid key's id as `mldsa65:` and 64 hex digits
//! ([`crate::sig::Id`]), optionally followed by a space and a comment.
//! Blank lines and lines starting with `#` are ignored. The two kinds never
//! match each other. An authinfo is three lines: `key <seed>`, `signer
//! <id>` and a certificate (`pgcert1`, or `pgcert2` for a hybrid key).
//!
//! A named key list (`/lib/keys/users`, `/lib/keys/servers`) has a name
//! before each key: `<name> <key> [comment]`, where the name is a user or
//! a host, or `*` for any. It binds a key to the names it may prove.

use alloc::string::String;
use alloc::vec::Vec;

use crate::cert::AnyCert;
use crate::sig::{Id, Secret};

pub const EBADINFO: &str = "bad authinfo";

/// Parses a key list. `None` when a line is malformed.
pub fn parse_list(text: &[u8]) -> Option<Vec<Id>> {
    let mut keys = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) || line.first() == Some(&b'#') {
            continue;
        }
        let end = line
            .iter()
            .position(|&b| b == b' ' || b == b'\t')
            .unwrap_or(line.len());
        keys.push(Id::parse(&line[..end])?);
    }
    Some(keys)
}

/// Like [`parse_list`], but skips malformed lines (for trust lists, where a
/// bad line must not grant anything and should not cancel the good ones).
pub fn parse_list_lenient(text: &[u8]) -> Vec<Id> {
    text.split(|&b| b == b'\n')
        .filter_map(|line| parse_list(line).and_then(|k| k.first().copied()))
        .collect()
}

/// The Ed25519 keys of a typed list, for the lists that hold only those
/// (module signers, Lethe's).
pub fn ed25519(ids: &[Id]) -> Vec<[u8; crate::PUBLIC]> {
    ids.iter()
        .filter_map(|i| match i {
            Id::Ed25519(k) => Some(*k),
            Id::Hybrid(_) => None,
        })
        .collect()
}

impl From<[u8; crate::PUBLIC]> for Id {
    fn from(k: [u8; crate::PUBLIC]) -> Id {
        Id::Ed25519(k)
    }
}

impl From<&[u8; crate::PUBLIC]> for Id {
    fn from(k: &[u8; crate::PUBLIC]) -> Id {
        Id::Ed25519(*k)
    }
}

impl From<&Id> for Id {
    fn from(k: &Id) -> Id {
        *k
    }
}

/// One line of a key list.
pub fn list_line(key: impl Into<Id>, comment: &str) -> String {
    let mut s = key.into().encode();
    if !comment.is_empty() {
        s.push(' ');
        s.push_str(comment);
    }
    s.push('\n');
    s
}

/// One line of a named key list: a key and the name it is trusted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    /// A user or host name, or [`ANY`].
    pub name: String,
    pub key: Id,
}

/// The name in a named key list that matches every name.
pub const ANY: &str = "*";

/// Parses a named key list: `<name> <key> [comment]` per line, blank
/// lines and lines starting with `#` ignored. A malformed line is skipped,
/// so it grants nothing and does not cancel the good ones.
pub fn parse_named(text: &[u8]) -> Vec<Named> {
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let Ok(line) = core::str::from_utf8(line) else {
            continue;
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_ascii_whitespace();
        let (Some(name), Some(key)) = (words.next(), words.next()) else {
            continue;
        };
        if let Some(key) = Id::parse(key.as_bytes()) {
            out.push(Named {
                name: String::from(name),
                key,
            });
        }
    }
    out
}

/// Whether `pattern`, a name from a named key list, covers `name`: it is
/// [`ANY`], or the same name with ASCII case ignored (host names are
/// case-insensitive; user names are compared the same way).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    pattern == ANY || pattern.eq_ignore_ascii_case(name)
}

/// The keys of `list` trusted for `name`; for `None`, every key in it.
pub fn keys_for(list: &[Named], name: Option<&str>) -> Vec<Id> {
    let mut keys = Vec::new();
    for n in list {
        if name.is_none_or(|name| name_matches(&n.name, name)) && !keys.contains(&n.key) {
            keys.push(n.key);
        }
    }
    keys
}

/// One line of a named key list.
pub fn named_line(name: &str, key: impl Into<Id>) -> String {
    alloc::format!("{name} {}\n", key.into().encode())
}

/// What `getauthinfo` and `mkauthinfo` write: the key, the issuing
/// signer and the certificate, all of one kind (Ed25519 with `pgcert1`,
/// or hybrid with `pgcert2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthInfo {
    pub key: Secret,
    pub signer: Id,
    pub cert: AnyCert,
}

impl AuthInfo {
    pub fn encode(&self) -> String {
        alloc::format!(
            "key {}\nsigner {}\n{}\n",
            self.key.encode(),
            self.signer.encode(),
            self.cert.encode()
        )
    }

    pub fn parse(text: &[u8]) -> Result<AuthInfo, &'static str> {
        let mut lines = text.split(|&b| b == b'\n');
        let key = lines
            .next()
            .and_then(|l| l.strip_prefix(b"key "))
            .and_then(Secret::parse)
            .ok_or(EBADINFO)?;
        let signer = lines
            .next()
            .and_then(|l| l.strip_prefix(b"signer "))
            .and_then(Id::parse)
            .ok_or(EBADINFO)?;
        let cert = AnyCert::parse(lines.next().ok_or(EBADINFO)?).map_err(|_| EBADINFO)?;
        if lines.any(|l| !l.is_empty()) || signer.is_hybrid() != key.is_hybrid() {
            return Err(EBADINFO);
        }
        // The certificate must be for this key. For a hybrid key only the
        // Ed25519 half is compared: deriving the ML-DSA half takes a key
        // generation and about 128 KiB of stack, more than a kernel
        // thread can spare where authinfo is read. A file whose ML-DSA
        // half does not match makes proofs no one accepts.
        let ok = match (&key, &cert) {
            (Secret::Ed25519(s), AnyCert::V1(c)) => c.key == crate::public_key(s),
            (Secret::Hybrid(h), AnyCert::V2(c)) => {
                c.key.0[..crate::PUBLIC] == crate::public_key(&h.ed25519())[..]
            }
            _ => false,
        };
        if !ok {
            return Err(EBADINFO);
        }
        Ok(AuthInfo { key, signer, cert })
    }

    /// The Ed25519 seed and `pgcert1` certificate of an Ed25519 authinfo,
    /// for what takes only those (Lethe, `#¤`).
    pub fn ed25519(&self) -> Option<([u8; crate::SEED], crate::cert::Cert)> {
        match (&self.key, &self.cert) {
            (Secret::Ed25519(s), AnyCert::V1(c)) => Some((*s, c.clone())),
            _ => None,
        }
    }
}

/// Finds a `pgcert1` certificate in `text`: the whole text, or a line of
/// it (an authinfo, for instance).
pub fn find_cert(text: &[u8]) -> Option<crate::cert::Cert> {
    match find_any_cert(text)? {
        AnyCert::V1(c) => Some(c),
        AnyCert::V2(_) => None,
    }
}

/// Finds a certificate of either kind in `text`.
pub fn find_any_cert(text: &[u8]) -> Option<AnyCert> {
    text.split(|&b| b == b'\n')
        .find(|l| l.starts_with(b"pgcert"))
        .and_then(|l| AnyCert::parse(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cert::{Cert, Cert2};
    use crate::sig::HybridSecret;
    use crate::{PUBLIC, SEED, hex};

    #[test]
    fn lists() {
        let a = Id::Ed25519([1u8; PUBLIC]);
        let b = Id::Ed25519([2u8; PUBLIC]);
        let h = Id::Hybrid([1u8; 32]);
        let text = alloc::format!(
            "# signers\n\n{}{}{}",
            list_line(a, "alpha"),
            list_line(b, ""),
            list_line(h, "hybrid")
        );
        assert!(text.contains("\nmldsa65:0101"));
        assert_eq!(parse_list(text.as_bytes()), Some(alloc::vec![a, b, h]));
        let bad = alloc::format!("{text}nothex\n");
        assert_eq!(parse_list(bad.as_bytes()), None);
        assert_eq!(parse_list_lenient(bad.as_bytes()), alloc::vec![a, b, h]);
        let glued = alloc::format!("{}x\n", hex::encode(&[1u8; PUBLIC]));
        assert_eq!(parse_list(glued.as_bytes()), None);
        assert_eq!(parse_list(b"mldsa65:\n"), None);
        assert_eq!(parse_list(b"mldsa65 0101\n"), None);
        assert_eq!(parse_list(b""), Some(Vec::new()));
        assert_eq!(
            ed25519(&[a, h, b]),
            alloc::vec![[1u8; PUBLIC], [2u8; PUBLIC]]
        );
        // A hybrid id never matches the Ed25519 key of the same bytes.
        assert_ne!(a, h);
    }

    #[test]
    fn named_lists() {
        let a = Id::Ed25519([1u8; PUBLIC]);
        let b = Id::Hybrid([2u8; PUBLIC]);
        let c = Id::Ed25519([3u8; PUBLIC]);
        let text = alloc::format!(
            "# servers\n\n{}{}  * {} any host\nnokey\n{} alone\nhost nothex\n",
            named_line("Host.example", a),
            named_line("other", b),
            c.encode(),
            a.encode(),
        );
        let list = parse_named(text.as_bytes());
        assert_eq!(list.len(), 3);
        assert_eq!(keys_for(&list, Some("host.EXAMPLE")), alloc::vec![a, c]);
        assert_eq!(keys_for(&list, Some("other")), alloc::vec![b, c]);
        assert_eq!(keys_for(&list, Some("nobody")), alloc::vec![c]);
        assert_eq!(keys_for(&list, None), alloc::vec![a, b, c]);
        assert!(name_matches("*", "x") && !name_matches("x", "*"));
    }

    #[test]
    fn authinfo_round_trip() {
        let seed = [3u8; SEED];
        let mut cert = Cert::new("bob", crate::public_key(&seed), 0).unwrap();
        cert.sign(&[4u8; SEED]).unwrap();
        let info = AuthInfo {
            key: Secret::Ed25519(seed),
            signer: Id::Ed25519(crate::public_key(&[4u8; SEED])),
            cert: AnyCert::V1(cert.clone()),
        };
        let text = info.encode();
        assert_eq!(AuthInfo::parse(text.as_bytes()).unwrap(), info);
        assert_eq!(find_cert(text.as_bytes()).unwrap(), cert);
        assert_eq!(info.ed25519(), Some((seed, cert)));
        let other = text.replace(&hex::encode(&seed), &hex::encode(&[5u8; SEED]));
        assert_eq!(AuthInfo::parse(other.as_bytes()), Err(EBADINFO));
    }

    #[test]
    fn hybrid_authinfo() {
        let user = HybridSecret([3; crate::sig::HYBRID_SEED]);
        let signer = HybridSecret([4; crate::sig::HYBRID_SEED]);
        let mut cert = Cert2::new("bob", user.public(), 0).unwrap();
        cert.sign(&signer).unwrap();
        let info = AuthInfo {
            key: Secret::Hybrid(user),
            signer: Id::Hybrid(signer.public().id()),
            cert: AnyCert::V2(cert),
        };
        let text = info.encode();
        assert!(text.starts_with("key mldsa65:"));
        assert!(text.contains("\nsigner mldsa65:"));
        assert!(text.contains("\npgcert2 bob mldsa65:"));
        let back = AuthInfo::parse(text.as_bytes()).unwrap();
        assert_eq!(back, info);
        assert_eq!(back.ed25519(), None);
        assert_eq!(find_cert(text.as_bytes()), None);
        assert_eq!(find_any_cert(text.as_bytes()), Some(info.cert.clone()));
        // A signer line of the other kind is refused.
        let i = info.signer.encode();
        let bad = text.replace(&i, &i[crate::sig::MARKER.len()..]);
        assert_eq!(AuthInfo::parse(bad.as_bytes()), Err(EBADINFO));
    }
}
