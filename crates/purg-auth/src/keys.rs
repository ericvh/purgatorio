//! Key lists and authinfo files.
//!
//! A key list has one public key per line: 64 hex digits, optionally a
//! space and a comment. Blank lines and lines starting with `#` are
//! ignored. An authinfo is three lines: `key <seed>`, `signer <public key>`
//! and a `pgcert1` certificate.

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
