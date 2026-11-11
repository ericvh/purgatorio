//! Key lists and authinfo files.
//!
//! A key list has one public key per line: 64 hex digits, optionally a
//! space and a comment. Blank lines and lines starting with `#` are
//! ignored. An authinfo is three lines: `key <seed>`, `signer <public key>`
//! and a `pgcert1` certificate.
//!
//! A named key list (`/lib/keys/users`, `/lib/keys/servers`) has a name
//! before each key: `<name> <key> [comment]`, where the name is a user or
//! a host, or `*` for any. It binds a key to the names it may prove.

use alloc::string::String;
use alloc::vec::Vec;

use crate::cert::Cert;
use crate::{PUBLIC, SEED, hex};

pub const EBADINFO: &str = "bad authinfo";

/// Parses a key list. `None` when a line is malformed.
pub fn parse_list(text: &[u8]) -> Option<Vec<[u8; PUBLIC]>> {
    let mut keys = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) || line.first() == Some(&b'#') {
            continue;
        }
        let (key, rest) = line.split_at(line.len().min(PUBLIC * 2));
        if !(rest.is_empty() || rest[0] == b' ' || rest[0] == b'\t') {
            return None;
        }
        keys.push(hex::array::<PUBLIC>(key)?);
    }
    Some(keys)
}

/// Like [`parse_list`], but skips malformed lines (for trust lists, where a
/// bad line must not grant anything and should not cancel the good ones).
pub fn parse_list_lenient(text: &[u8]) -> Vec<[u8; PUBLIC]> {
    text.split(|&b| b == b'\n')
        .filter_map(|line| parse_list(line).and_then(|k| k.first().copied()))
        .collect()
}

/// One line of a key list.
pub fn list_line(key: &[u8; PUBLIC], comment: &str) -> String {
    let mut s = hex::encode(key);
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
    pub key: [u8; PUBLIC],
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
        if let Some(key) = hex::array::<PUBLIC>(key.as_bytes()) {
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
pub fn keys_for(list: &[Named], name: Option<&str>) -> Vec<[u8; PUBLIC]> {
    let mut keys = Vec::new();
    for n in list {
        if name.is_none_or(|name| name_matches(&n.name, name)) && !keys.contains(&n.key) {
            keys.push(n.key);
        }
    }
    keys
}

/// One line of a named key list.
pub fn named_line(name: &str, key: &[u8; PUBLIC]) -> String {
    alloc::format!("{name} {}\n", hex::encode(key))
}

/// What `getauthinfo` and `mkauthinfo` write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthInfo {
    pub seed: [u8; SEED],
    pub signer: [u8; PUBLIC],
    pub cert: Cert,
}

impl AuthInfo {
    pub fn encode(&self) -> String {
        alloc::format!(
            "key {}\nsigner {}\n{}\n",
            hex::encode(&self.seed),
            hex::encode(&self.signer),
            self.cert.encode()
        )
    }

    pub fn parse(text: &[u8]) -> Result<AuthInfo, &'static str> {
        let mut lines = text.split(|&b| b == b'\n');
        let seed = lines
            .next()
            .and_then(|l| l.strip_prefix(b"key "))
            .and_then(hex::array::<SEED>)
            .ok_or(EBADINFO)?;
        let signer = lines
            .next()
            .and_then(|l| l.strip_prefix(b"signer "))
            .and_then(hex::array::<PUBLIC>)
            .ok_or(EBADINFO)?;
        let cert = Cert::parse(lines.next().ok_or(EBADINFO)?).map_err(|_| EBADINFO)?;
        if lines.any(|l| !l.is_empty()) || cert.key != crate::public_key(&seed) {
            return Err(EBADINFO);
        }
        Ok(AuthInfo { seed, signer, cert })
    }
}

/// Finds a certificate in `text`: the whole text, or a line of it (an
/// authinfo, for instance).
pub fn find_cert(text: &[u8]) -> Option<Cert> {
    text.split(|&b| b == b'\n')
        .find(|l| l.starts_with(b"pgcert"))
        .and_then(|l| Cert::parse(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists() {
        let a = [1u8; PUBLIC];
        let b = [2u8; PUBLIC];
        let text = alloc::format!(
            "# signers\n\n{}{}",
            list_line(&a, "alpha"),
            list_line(&b, "")
        );
        assert_eq!(parse_list(text.as_bytes()), Some(alloc::vec![a, b]));
        let bad = alloc::format!("{text}nothex\n");
        assert_eq!(parse_list(bad.as_bytes()), None);
        assert_eq!(parse_list_lenient(bad.as_bytes()), alloc::vec![a, b]);
        let glued = alloc::format!("{}x\n", hex::encode(&a));
        assert_eq!(parse_list(glued.as_bytes()), None);
        assert_eq!(parse_list(b""), Some(Vec::new()));
    }

    #[test]
    fn named_lists() {
        let a = [1u8; PUBLIC];
        let b = [2u8; PUBLIC];
        let c = [3u8; PUBLIC];
        let text = alloc::format!(
            "# servers\n\n{}{}  * {} any host\nnokey\n{} alone\nhost nothex\n",
            named_line("Host.example", &a),
            named_line("other", &b),
            hex::encode(&c),
            hex::encode(&a),
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
            seed,
            signer: crate::public_key(&[4u8; SEED]),
            cert,
        };
        let text = info.encode();
        assert_eq!(AuthInfo::parse(text.as_bytes()).unwrap(), info);
        assert_eq!(find_cert(text.as_bytes()).unwrap(), info.cert);
        let other = text.replace(&hex::encode(&seed), &hex::encode(&[5u8; SEED]));
        assert_eq!(AuthInfo::parse(other.as_bytes()), Err(EBADINFO));
    }
}
