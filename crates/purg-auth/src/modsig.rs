//! Module digests and signatures (`docs/signing.md`).
//!
//! A module's digest is SHA-256 over its bytes with every `purgatorio.sig`
//! custom section cut out. That section holds one or more entries, each an
//! Ed25519 signature over [`message`] with either the bare signer key or a
//! chain of `pgcert1` certificates. [`check`] decides whether a module
//! passes for a set of trust anchors and a revocation list.

use alloc::vec::Vec;
use core::ops::Range;

use crate::sha256::Sha256;

use crate::cert::Cert;
use crate::{PUBLIC, SEED, SIGNATURE};

/// The custom section that holds a module's signatures.
pub const SECTION: &str = "purgatorio.sig";
/// The first bytes of every signed message.
pub const CONTEXT: &[u8] = b"pgmodsig1\0";
/// The section format this code reads and writes.
pub const VERSION: u8 = 1;
/// A SHA-256 digest.
pub const DIGEST: usize = 32;
/// At most this many entries in a section.
pub const MAX_ENTRIES: usize = 8;
/// At most this many certificates in an entry's chain.
pub const MAX_CERTS: usize = 4;
/// A certificate line is at most this long.
pub const MAX_CERT_LEN: usize = 2048;

const KIND_KEY: u8 = 0;
const KIND_CERTS: u8 = 1;
const HEADER: &[u8] = b"\0asm\x01\0\0\0";

pub const NOT_SIGNED: &str = "module not signed";
pub const BAD_SIGNATURE: &str = "bad signature";
pub const MODULE_REVOKED: &str = "module revoked";
pub use purg_abi::auth::{EXPIRED, REVOKED, UNTRUSTED};
pub const ETOOMANY: &str = "too many signatures";
pub const EBADCHAIN: &str = "bad certificate chain";

/// A byte string that is not a well-formed WASM section list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// An unsigned LEB128 `u32`: at most 5 bytes, and a fifth byte carries
/// only the top 4 bits.
fn leb_u32(buf: &[u8], pos: &mut usize) -> Result<u32, Malformed> {
    let mut v: u32 = 0;
    for i in 0..5 {
        let b = *buf.get(*pos).ok_or(Malformed)?;
        *pos += 1;
        if i == 4 && b > 0x0f {
            return Err(Malformed);
        }
        v |= u32::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Malformed)
}

fn put_leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// One section of a module.
struct Section<'a> {
    /// The whole section in the file, from its id byte.
    range: Range<usize>,
    /// A custom section's name and what follows it.
    custom: Option<(&'a [u8], &'a [u8])>,
}

/// Walks the section list. The list must end exactly at the end of `wasm`.
fn sections(wasm: &[u8]) -> Result<Vec<Section<'_>>, Malformed> {
    if wasm.get(..HEADER.len()) != Some(HEADER) {
        return Err(Malformed);
    }
    let mut out = Vec::new();
    let mut pos = HEADER.len();
    while pos < wasm.len() {
        let start = pos;
        let id = wasm[pos];
        pos += 1;
        let size = leb_u32(wasm, &mut pos)? as usize;
        let end = pos
            .checked_add(size)
            .filter(|&e| e <= wasm.len())
            .ok_or(Malformed)?;
        let custom = if id == 0 {
            let body = &wasm[pos..end];
            let mut p = 0;
            let nlen = leb_u32(body, &mut p)? as usize;
            let nend = p.checked_add(nlen).ok_or(Malformed)?;
            Some((body.get(p..nend).ok_or(Malformed)?, &body[nend..]))
        } else {
            None
        };
        out.push(Section {
            range: start..end,
            custom,
        });
        pos = end;
    }
    Ok(out)
}

/// What the loader needs to know about a module's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info<'a> {
    pub digest: [u8; DIGEST],
    /// The contents of the first `purgatorio.abi` section, or empty.
    pub abi: &'a [u8],
    /// The contents of each `purgatorio.sig` section, after the name.
    pub sigs: Vec<&'a [u8]>,
}

/// Computes the digest of `wasm` and finds its ABI and signature sections.
pub fn inspect(wasm: &[u8]) -> Result<Info<'_>, Malformed> {
    let mut hash = Sha256::new();
    hash.update(HEADER);
    let mut abi: Option<&[u8]> = None;
    let mut sigs = Vec::new();
    for s in sections(wasm)? {
        match s.custom {
            Some((name, body)) if name == SECTION.as_bytes() => {
                sigs.push(body);
                continue;
            }
            Some((name, body)) if name == purg_abi::ABI_SECTION.as_bytes() => {
                abi.get_or_insert(body);
            }
            _ => {}
        }
        hash.update(&wasm[s.range]);
    }
    Ok(Info {
        digest: hash.finalize(),
        abi: abi.unwrap_or(&[]),
        sigs,
    })
}

/// The digest of `wasm`: SHA-256 without its `purgatorio.sig` sections.
pub fn digest(wasm: &[u8]) -> Result<[u8; DIGEST], Malformed> {
    inspect(wasm).map(|i| i.digest)
}

/// `wasm` without its `purgatorio.sig` sections.
pub fn strip(wasm: &[u8]) -> Result<Vec<u8>, Malformed> {
    let mut out = Vec::with_capacity(wasm.len());
    out.extend_from_slice(HEADER);
    for s in sections(wasm)? {
        if !matches!(s.custom, Some((name, _)) if name == SECTION.as_bytes()) {
            out.extend_from_slice(&wasm[s.range]);
        }
    }
    Ok(out)
}

/// The bytes a signer signs for a module with this digest and ABI
/// version. `None` when `abi` is longer than 255 bytes.
pub fn message(digest: &[u8; DIGEST], abi: &[u8]) -> Option<Vec<u8>> {
    let len = u8::try_from(abi.len()).ok()?;
    let mut m = Vec::with_capacity(CONTEXT.len() + DIGEST + 1 + abi.len());
    m.extend_from_slice(CONTEXT);
    m.extend_from_slice(digest);
    m.push(len);
    m.extend_from_slice(abi);
    Some(m)
}

/// Who vouches for an entry's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signer {
    /// A bare key.
    Key([u8; PUBLIC]),
    /// A chain: the first certificate is for the signing key, and each
    /// later one is for a key that signed the one before it.
    Certs(Vec<Cert>),
}

/// One signature in a `purgatorio.sig` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub signature: [u8; SIGNATURE],
    pub signer: Signer,
}

impl Entry {
    /// An entry around a signature made elsewhere (by an agent). `certs`
    /// is the chain for `key`, or empty for a bare key.
    pub fn new(
        key: [u8; PUBLIC],
        signature: [u8; SIGNATURE],
        certs: Vec<Cert>,
    ) -> Result<Entry, &'static str> {
        let signer = if certs.is_empty() {
            Signer::Key(key)
        } else {
            if certs.len() > MAX_CERTS
                || certs[0].key != key
                || certs
                    .iter()
                    .any(|c| c.encode().len() > MAX_CERT_LEN || c.sigs.is_empty())
            {
                return Err(EBADCHAIN);
            }
            Signer::Certs(certs)
        };
        Ok(Entry { signature, signer })
    }

    /// Signs `digest` and `abi` with `seed`.
    pub fn sign(
        seed: &[u8; SEED],
        certs: Vec<Cert>,
        digest: &[u8; DIGEST],
        abi: &[u8],
    ) -> Result<Entry, &'static str> {
        let msg = message(digest, abi).ok_or(BAD_SIGNATURE)?;
        Entry::new(crate::public_key(seed), crate::sign(seed, &msg), certs)
    }

    /// The key the signature is by.
    pub fn key(&self) -> &[u8; PUBLIC] {
        match &self.signer {
            Signer::Key(k) => k,
            Signer::Certs(c) => &c[0].key,
        }
    }

    /// The certificates, first the signing key's. Empty for a bare key.
    pub fn certs(&self) -> &[Cert] {
        match &self.signer {
            Signer::Key(_) => &[],
            Signer::Certs(c) => c,
        }
    }

    /// The entry as it is stored in the section.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match &self.signer {
            Signer::Key(key) => {
                out.push(KIND_KEY);
                out.extend_from_slice(&self.signature);
                out.extend_from_slice(key);
            }
            Signer::Certs(certs) => {
                out.push(KIND_CERTS);
                out.extend_from_slice(&self.signature);
                out.push(certs.len() as u8);
                for c in certs {
                    let line = c.encode();
                    out.extend_from_slice(&(line.len() as u16).to_le_bytes());
                    out.extend_from_slice(line.as_bytes());
                }
            }
        }
        out
    }

    /// Parses one entry from the front of `buf`, returning what is left.
    fn take(buf: &[u8]) -> Option<(Entry, &[u8])> {
        let (&kind, rest) = buf.split_first()?;
        let (signature, rest) = rest.split_first_chunk::<SIGNATURE>()?;
        let (signer, rest) = match kind {
            KIND_KEY => {
                let (key, rest) = rest.split_first_chunk::<PUBLIC>()?;
                (Signer::Key(*key), rest)
            }
            KIND_CERTS => {
                let (&n, mut rest) = rest.split_first()?;
                if n == 0 || usize::from(n) > MAX_CERTS {
                    return None;
                }
                let mut certs = Vec::new();
                for _ in 0..n {
                    let (len, r) = rest.split_first_chunk::<2>()?;
                    let len = usize::from(u16::from_le_bytes(*len));
                    if len == 0 || len > MAX_CERT_LEN || len > r.len() {
                        return None;
                    }
                    let (line, r) = r.split_at(len);
                    // `Cert::parse` allows one trailing newline; the
                    // section does not.
                    if line.ends_with(b"\n") {
                        return None;
                    }
                    certs.push(Cert::parse(line).ok()?);
                    rest = r;
                }
                (Signer::Certs(certs), rest)
            }
            _ => return None,
        };
        Some((
            Entry {
                signature: *signature,
                signer,
            },
            rest,
        ))
    }

    /// Parses exactly one entry (factotum's reply).
    pub fn parse(buf: &[u8]) -> Option<Entry> {
        match Entry::take(buf)? {
            (e, []) => Some(e),
            _ => None,
        }
    }
}

/// Encodes the contents of a `purgatorio.sig` section (after its name).
pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = alloc::vec![VERSION, entries.len() as u8];
    for e in entries {
        out.extend_from_slice(&e.encode());
    }
    out
}

/// Parses the contents of a `purgatorio.sig` section. Strict: `None` for
/// anything [`encode`] would not write.
pub fn parse(body: &[u8]) -> Option<Vec<Entry>> {
    let (&[version, count], mut rest) = body.split_first_chunk::<2>()?;
    if version != VERSION || count == 0 || usize::from(count) > MAX_ENTRIES {
        return None;
    }
    let mut entries: Vec<Entry> = Vec::new();
    for _ in 0..count {
        let (e, r) = Entry::take(rest)?;
        if entries.iter().any(|x| x.key() == e.key()) {
            return None;
        }
        entries.push(e);
        rest = r;
    }
    rest.is_empty().then_some(entries)
}

/// The entries of a module: empty when it is unsigned, an error when it
/// has more than one signature section or a malformed one.
pub fn entries(info: &Info<'_>) -> Result<Vec<Entry>, &'static str> {
    match info.sigs.as_slice() {
        [] => Ok(Vec::new()),
        [one] => parse(one).ok_or(BAD_SIGNATURE),
        _ => Err(BAD_SIGNATURE),
    }
}

/// Returns `wasm` carrying `entry`: its other entries are kept, an entry
/// by the same key is replaced, and the one signature section is written
/// last.
pub fn attach(wasm: &[u8], entry: Entry) -> Result<Vec<u8>, &'static str> {
    let info = inspect(wasm).map_err(|Malformed| purg_abi::error::EBADEXEC)?;
    let mut all = entries(&info)?;
    all.retain(|e| e.key() != entry.key());
    if all.len() >= MAX_ENTRIES {
        return Err(ETOOMANY);
    }
    all.push(entry);
    let body = encode(&all);
    let mut out = strip(wasm).map_err(|Malformed| purg_abi::error::EBADEXEC)?;
    let mut name = Vec::new();
    put_leb(&mut name, SECTION.len() as u32);
    name.extend_from_slice(SECTION.as_bytes());
    let size = u32::try_from(name.len() + body.len()).map_err(|_| ETOOMANY)?;
    out.push(0);
    put_leb(&mut out, size);
    out.extend_from_slice(&name);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Signs `wasm` with `seed` and returns the signed module.
pub fn sign(wasm: &[u8], seed: &[u8; SEED], certs: Vec<Cert>) -> Result<Vec<u8>, &'static str> {
    let info = inspect(wasm).map_err(|Malformed| purg_abi::error::EBADEXEC)?;
    let entry = Entry::sign(seed, certs, &info.digest, info.abi)?;
    attach(wasm, entry)
}

/// What a module policy trusts.
pub struct Trust<'a> {
    /// `/lib/keys/modules`: keys that may sign modules or certify signers.
    pub anchors: &'a [[u8; PUBLIC]],
    /// `/lib/keys/revoked`: keys and module digests. `None` when the list
    /// is damaged: nothing then passes.
    pub revoked: Option<&'a [[u8; PUBLIC]]>,
    /// Seconds since 1970 UTC.
    pub now: u64,
}

impl Trust<'_> {
    fn revoked(&self, key: &[u8; PUBLIC]) -> bool {
        self.revoked.is_none_or(|r| r.contains(key))
    }
}

/// How one entry stands. Ordered from worst to best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    /// The signature does not verify.
    Bad,
    Untrusted,
    Expired,
    Revoked,
    Trusted,
}

impl Standing {
    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Bad => "bad",
            Standing::Untrusted => "untrusted",
            Standing::Expired => "expired",
            Standing::Revoked => "revoked",
            Standing::Trusted => "trusted",
        }
    }

    /// The loader's error when this is the best any entry does.
    fn error(self) -> &'static str {
        match self {
            Standing::Bad | Standing::Trusted => BAD_SIGNATURE,
            Standing::Untrusted => UNTRUSTED,
            Standing::Expired => EXPIRED,
            Standing::Revoked => REVOKED,
        }
    }
}

/// Judges one entry of a module whose signed message is `msg`.
pub fn standing(entry: &Entry, msg: &[u8], trust: &Trust<'_>) -> Standing {
    if !crate::verify(entry.key(), msg, &entry.signature) {
        return Standing::Bad;
    }
    let certs = match &entry.signer {
        Signer::Key(key) => {
            return if trust.revoked(key) {
                Standing::Revoked
            } else if trust.anchors.contains(key) {
                Standing::Trusted
            } else {
                Standing::Untrusted
            };
        }
        Signer::Certs(certs) => certs,
    };
    if certs.iter().any(|c| trust.revoked(&c.key)) {
        return Standing::Revoked;
    }
    if trust.anchors.contains(&certs[0].key) {
        return Standing::Trusted;
    }
    if certs.iter().any(|c| c.expired(trust.now)) {
        return Standing::Expired;
    }
    let mut revoked_anchor = false;
    for (i, c) in certs.iter().enumerate() {
        for (s, _) in &c.sigs {
            if trust.anchors.contains(s) && c.signed_by(s) {
                if !trust.revoked(s) {
                    return Standing::Trusted;
                }
                revoked_anchor = true;
            }
        }
        if !certs.get(i + 1).is_some_and(|next| c.signed_by(&next.key)) {
            break;
        }
    }
    if revoked_anchor {
        Standing::Revoked
    } else {
        Standing::Untrusted
    }
}

/// Checks a module against `trust`. Returns the signing keys of its
/// trusted entries, or the error a `require` policy refuses it with.
pub fn check(info: &Info<'_>, trust: &Trust<'_>) -> Result<Vec<[u8; PUBLIC]>, &'static str> {
    if info.sigs.is_empty() {
        return Err(NOT_SIGNED);
    }
    if trust.revoked.is_some_and(|r| r.contains(&info.digest)) {
        return Err(MODULE_REVOKED);
    }
    let entries = entries(info)?;
    let msg = message(&info.digest, info.abi).ok_or(BAD_SIGNATURE)?;
    let mut best = Standing::Bad;
    let mut signers = Vec::new();
    for e in &entries {
        let s = standing(e, &msg, trust);
        if s == Standing::Trusted {
            signers.push(*e.key());
        }
        best = best.max(s);
    }
    if signers.is_empty() {
        Err(best.error())
    } else {
        Ok(signers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex;

    const KEY: [u8; SEED] = [0x42; SEED];
    const ROOT: [u8; SEED] = [3; SEED];

    fn section(id: u8, body: &[u8]) -> Vec<u8> {
        let mut s = alloc::vec![id];
        put_leb(&mut s, body.len() as u32);
        s.extend_from_slice(body);
        s
    }

    fn custom(name: &str, body: &[u8]) -> Vec<u8> {
        let mut b = alloc::vec![name.len() as u8];
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(body);
        section(0, &b)
    }

    /// A type section, the ABI section and a data-like section.
    fn parts() -> [Vec<u8>; 3] {
        [
            section(1, &[1, 0x60, 0, 0]),
            custom("purgatorio.abi", b"0.6"),
            section(11, b"some data"),
        ]
    }

    fn module() -> Vec<u8> {
        let mut m = HEADER.to_vec();
        for p in parts() {
            m.extend_from_slice(&p);
        }
        m
    }

    fn trust<'a>(anchors: &'a [[u8; PUBLIC]], revoked: &'a [[u8; PUBLIC]]) -> Trust<'a> {
        Trust {
            anchors,
            revoked: Some(revoked),
            now: 1000,
        }
    }

    fn cert(name: &str, key: &[u8; SEED], expires: u64, by: &[u8; SEED]) -> Cert {
        let mut c = Cert::new(name, crate::public_key(key), expires).unwrap();
        c.sign(by).unwrap();
        c
    }

    fn verdict(wasm: &[u8], t: &Trust<'_>) -> Result<Vec<[u8; PUBLIC]>, &'static str> {
        check(&inspect(wasm).unwrap(), t)
    }

    #[test]
    fn leb128() {
        for v in [0u32, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut b = Vec::new();
            put_leb(&mut b, v);
            let mut pos = 0;
            assert_eq!(leb_u32(&b, &mut pos), Ok(v));
            assert_eq!(pos, b.len());
        }
        // Redundant bytes are read; a fifth byte with high bits is not.
        assert_eq!(leb_u32(&[0x81, 0x80, 0x00], &mut 0), Ok(1));
        assert_eq!(
            leb_u32(&[0xff, 0xff, 0xff, 0xff, 0x0f], &mut 0),
            Ok(u32::MAX)
        );
        assert_eq!(
            leb_u32(&[0xff, 0xff, 0xff, 0xff, 0x1f], &mut 0),
            Err(Malformed)
        );
        assert_eq!(
            leb_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0], &mut 0),
            Err(Malformed)
        );
        assert_eq!(leb_u32(&[0x80], &mut 0), Err(Malformed));
    }

    #[test]
    fn the_digest_ignores_signature_sections_wherever_they_sit() {
        let plain = module();
        let want = digest(&plain).unwrap();
        let direct: [u8; DIGEST] = <sha2::Sha256 as sha2::Digest>::digest(&plain).into();
        assert_eq!(want, direct);
        let sig = custom(SECTION, b"anything at all");
        let p = parts();
        for at in 0..=p.len() {
            let mut m = HEADER.to_vec();
            for (i, part) in p.iter().enumerate() {
                if i == at {
                    m.extend_from_slice(&sig);
                }
                m.extend_from_slice(part);
            }
            if at == p.len() {
                m.extend_from_slice(&sig);
            }
            let info = inspect(&m).unwrap();
            assert_eq!(info.digest, want, "section at {at}");
            assert_eq!(info.abi, b"0.6");
            assert_eq!(info.sigs, alloc::vec![&b"anything at all"[..]]);
            assert_eq!(strip(&m).unwrap(), plain);
        }
        // Several of them, and nothing else.
        let mut m = HEADER.to_vec();
        m.extend_from_slice(&sig);
        m.extend_from_slice(&sig);
        assert_eq!(inspect(&m).unwrap().sigs.len(), 2);
        assert_eq!(digest(&m), digest(HEADER));
        // Any other change shows.
        let mut other = plain.clone();
        *other.last_mut().unwrap() ^= 1;
        assert_ne!(digest(&other).unwrap(), want);
        let mut more = plain.clone();
        more.extend_from_slice(&custom("purgatorio.sig2", b"x"));
        assert_ne!(digest(&more).unwrap(), want);
    }

    #[test]
    fn redundant_leb128_is_hashed_as_written() {
        let mut m = HEADER.to_vec();
        m.extend_from_slice(&[11, 0x82, 0x00, b'h', b'i']);
        let direct: [u8; DIGEST] = <sha2::Sha256 as sha2::Digest>::digest(&m).into();
        assert_eq!(digest(&m).unwrap(), direct);
        let mut short = HEADER.to_vec();
        short.extend_from_slice(&[11, 0x02, b'h', b'i']);
        assert_ne!(digest(&short).unwrap(), direct);
    }

    #[test]
    fn malformed_modules_have_no_digest() {
        assert_eq!(digest(b""), Err(Malformed));
        assert_eq!(digest(b"#!/bin/sh\n"), Err(Malformed));
        assert_eq!(digest(b"\0asm\x02\0\0\0"), Err(Malformed));
        let m = module();
        // Every truncation either is a shorter valid section list or is
        // malformed; none panics.
        let mut valid = 0;
        for n in 0..m.len() {
            if digest(&m[..n]).is_ok() {
                valid += 1;
            }
        }
        assert_eq!(valid, parts().len());
        // Bytes after the last section, and a custom section whose name
        // runs past its end.
        let mut tail = m.clone();
        tail.push(0);
        assert_eq!(digest(&tail), Err(Malformed));
        let mut bad = HEADER.to_vec();
        bad.extend_from_slice(&[0, 2, 5, b'x']);
        assert_eq!(digest(&bad), Err(Malformed));
        let mut huge = HEADER.to_vec();
        huge.extend_from_slice(&[1, 0xff, 0xff, 0xff, 0xff, 0x0f, 0]);
        assert_eq!(digest(&huge), Err(Malformed));
    }

    #[test]
    fn sign_and_check_a_bare_key() {
        let plain = module();
        let signed = sign(&plain, &KEY, Vec::new()).unwrap();
        assert_eq!(strip(&signed).unwrap(), plain);
        assert_eq!(digest(&signed), digest(&plain));
        // Signing is deterministic, and signing again changes nothing.
        assert_eq!(sign(&plain, &KEY, Vec::new()).unwrap(), signed);
        assert_eq!(sign(&signed, &KEY, Vec::new()).unwrap(), signed);
        let key = crate::public_key(&KEY);
        assert_eq!(verdict(&signed, &trust(&[key], &[])), Ok(alloc::vec![key]));
        assert_eq!(verdict(&plain, &trust(&[key], &[])), Err(NOT_SIGNED));
        assert_eq!(verdict(&signed, &trust(&[], &[])), Err(UNTRUSTED));
        assert_eq!(verdict(&signed, &trust(&[key], &[key])), Err(REVOKED));
        let d = digest(&plain).unwrap();
        assert_eq!(verdict(&signed, &trust(&[key], &[d])), Err(MODULE_REVOKED));
        let damaged = Trust {
            anchors: &[key],
            revoked: None,
            now: 0,
        };
        assert_eq!(verdict(&signed, &damaged), Err(REVOKED));
        assert_eq!(verdict(&plain, &damaged), Err(NOT_SIGNED));
    }

    #[test]
    fn a_fixed_vector() {
        // The public test seed over a three-section module: the same
        // bytes on every host and in every implementation.
        let signed = sign(&module(), &KEY, Vec::new()).unwrap();
        assert_eq!(
            hex::encode(&digest(&signed).unwrap()),
            "8d231c88d5f3252c086c9d1a768229990e7700be60fd016d65749f37284b695c"
        );
        let body = inspect(&signed).unwrap().sigs[0];
        assert_eq!(body.len(), 2 + 1 + SIGNATURE + PUBLIC);
        assert_eq!(&body[..3], &[VERSION, 1, KIND_KEY]);
        assert_eq!(
            hex::encode(&body[3..3 + SIGNATURE]),
            "a087e76b56bb910850fab44dcba17617b03290eed37e83432e8dcae374374f87cfba1b09bb1cf020fea9e635c8e1bb686ea8665c197daebbcc9951f5e23e6903"
        );
        assert_eq!(&body[3 + SIGNATURE..], &crate::public_key(&KEY));
        assert!(signed.ends_with(body));
    }

    #[test]
    fn tampering_breaks_the_signature() {
        let signed = sign(&module(), &KEY, Vec::new()).unwrap();
        let key = crate::public_key(&KEY);
        let t = trust(core::slice::from_ref(&key), &[]);
        // The signature section is the last one.
        let sec = module().len();
        // A changed byte in any other section.
        for at in HEADER.len()..sec {
            let mut m = signed.clone();
            m[at] ^= 0x20;
            if let Ok(info) = inspect(&m) {
                assert_ne!(check(&info, &t), Ok(alloc::vec![key]), "byte {at}");
            }
        }
        // A section added after signing, before or after the signature.
        let mut m = signed.clone();
        m.extend_from_slice(&custom("extra", b"x"));
        assert_eq!(verdict(&m, &t), Err(BAD_SIGNATURE));
        let mut m = module();
        m.extend_from_slice(&custom("extra", b"x"));
        m.extend_from_slice(&signed[module().len()..]);
        assert_eq!(verdict(&m, &t), Err(BAD_SIGNATURE));
        // A changed ABI section is a changed module.
        let other = signed
            .windows(3)
            .position(|w| w == b"0.6")
            .map(|i| {
                let mut m = signed.clone();
                m[i + 2] = b'7';
                m
            })
            .unwrap();
        assert_eq!(verdict(&other, &t), Err(BAD_SIGNATURE));
        // A second signature section.
        let mut m = signed.clone();
        m.extend_from_slice(&signed[sec..]);
        assert_eq!(inspect(&m).unwrap().sigs.len(), 2);
        assert_eq!(verdict(&m, &t), Err(BAD_SIGNATURE));
        // The signature moved to the front still verifies.
        let mut m = HEADER.to_vec();
        m.extend_from_slice(&signed[sec..]);
        m.extend_from_slice(&module()[HEADER.len()..]);
        assert_eq!(verdict(&m, &t), Ok(alloc::vec![key]));
    }

    #[test]
    fn countersigning() {
        let other = [5u8; SEED];
        let one = sign(&module(), &KEY, Vec::new()).unwrap();
        let two = sign(&one, &other, Vec::new()).unwrap();
        assert_eq!(inspect(&two).unwrap().sigs.len(), 1);
        let (a, b) = (crate::public_key(&KEY), crate::public_key(&other));
        assert_eq!(verdict(&two, &trust(&[a], &[])), Ok(alloc::vec![a]));
        assert_eq!(verdict(&two, &trust(&[b], &[])), Ok(alloc::vec![b]));
        assert_eq!(verdict(&two, &trust(&[a, b], &[])), Ok(alloc::vec![a, b]));
        // One revoked signer does not spoil the other.
        assert_eq!(verdict(&two, &trust(&[a, b], &[a])), Ok(alloc::vec![b]));
        assert_eq!(verdict(&two, &trust(&[a], &[a])), Err(REVOKED));
        // Eight entries at most.
        let mut m = module();
        for i in 0..MAX_ENTRIES {
            m = sign(&m, &[10 + i as u8; SEED], Vec::new()).unwrap();
        }
        assert_eq!(sign(&m, &KEY, Vec::new()), Err(ETOOMANY));
        assert_eq!(sign(&m, &[10; SEED], Vec::new()).unwrap().len(), m.len());
    }

    #[test]
    fn certificate_chains() {
        let mid = [4u8; SEED];
        let root = crate::public_key(&ROOT);
        let direct = alloc::vec![cert("builder", &KEY, 0, &ROOT)];
        let signed = sign(&module(), &KEY, direct).unwrap();
        let key = crate::public_key(&KEY);
        assert_eq!(verdict(&signed, &trust(&[root], &[])), Ok(alloc::vec![key]));
        assert_eq!(verdict(&signed, &trust(&[], &[])), Err(UNTRUSTED));
        assert_eq!(verdict(&signed, &trust(&[root], &[root])), Err(REVOKED));
        assert_eq!(verdict(&signed, &trust(&[root], &[key])), Err(REVOKED));
        // A held key needs no signer.
        assert_eq!(verdict(&signed, &trust(&[key], &[])), Ok(alloc::vec![key]));

        let chain = alloc::vec![
            cert("builder", &KEY, 2000, &mid),
            cert("release", &mid, 0, &ROOT),
        ];
        let signed = sign(&module(), &KEY, chain).unwrap();
        assert_eq!(verdict(&signed, &trust(&[root], &[])), Ok(alloc::vec![key]));
        let midpub = crate::public_key(&mid);
        assert_eq!(
            verdict(&signed, &trust(&[midpub], &[])),
            Ok(alloc::vec![key])
        );
        assert_eq!(verdict(&signed, &trust(&[root], &[midpub])), Err(REVOKED));
        let late = Trust {
            anchors: &[root],
            revoked: Some(&[]),
            now: 2000,
        };
        assert_eq!(verdict(&signed, &late), Err(EXPIRED));

        // A chain that does not link: the second certificate is for a key
        // that did not sign the first.
        let broken = alloc::vec![
            cert("builder", &KEY, 0, &mid),
            cert("release", &[6; SEED], 0, &ROOT),
        ];
        let signed = sign(&module(), &KEY, broken).unwrap();
        assert_eq!(verdict(&signed, &trust(&[root], &[])), Err(UNTRUSTED));
        // A certificate for another key cannot head the chain.
        assert_eq!(
            sign(&module(), &KEY, alloc::vec![cert("x", &mid, 0, &ROOT)]),
            Err(EBADCHAIN)
        );
        let five = alloc::vec![cert("builder", &KEY, 0, &ROOT); 5];
        assert_eq!(sign(&module(), &KEY, five), Err(EBADCHAIN));
        // A forged certificate signature is untrusted.
        let mut forged = cert("builder", &KEY, 0, &mid);
        forged.sigs[0].0 = root;
        let signed = sign(&module(), &KEY, alloc::vec![forged]).unwrap();
        assert_eq!(verdict(&signed, &trust(&[root], &[])), Err(UNTRUSTED));
    }

    #[test]
    fn entries_round_trip_and_parse_strictly() {
        let d = digest(&module()).unwrap();
        let bare = Entry::sign(&KEY, Vec::new(), &d, b"0.6").unwrap();
        let chained = Entry::sign(
            &[5; SEED],
            alloc::vec![
                cert("a", &[5; SEED], 0, &[4; SEED]),
                cert("b", &[4; SEED], 9, &ROOT)
            ],
            &d,
            b"0.6",
        )
        .unwrap();
        let all = alloc::vec![bare.clone(), chained.clone()];
        let body = encode(&all);
        assert_eq!(parse(&body), Some(all));
        assert_eq!(Entry::parse(&chained.encode()), Some(chained.clone()));
        assert_eq!(Entry::parse(&body), None);

        // Every truncation and every extension is rejected.
        for n in 0..body.len() {
            assert_eq!(parse(&body[..n]), None, "length {n}");
        }
        let mut long = body.clone();
        long.push(0);
        assert_eq!(parse(&long), None);
        // Version, count, kind, duplicate keys.
        for (at, v) in [(0, 2u8), (1, 0), (1, 3), (1, 9), (2, 7)] {
            let mut b = body.clone();
            b[at] = v;
            assert_eq!(parse(&b), None, "byte {at} = {v}");
        }
        assert_eq!(parse(&encode(&[bare.clone(), bare.clone()])), None);
        // Certificate count and length, and a trailing newline.
        let one = encode(core::slice::from_ref(&chained));
        let ncerts = 2 + 1 + SIGNATURE;
        for v in [0u8, 1, 3, 5] {
            let mut b = one.clone();
            b[ncerts] = v;
            assert_eq!(parse(&b), None, "ncerts {v}");
        }
        let mut b = one.clone();
        b[ncerts + 1] = 0;
        b[ncerts + 2] = 0;
        assert_eq!(parse(&b), None);
        let Signer::Certs(certs) = &chained.signer else {
            unreachable!()
        };
        let mut nl = alloc::vec![VERSION, 1, KIND_CERTS];
        nl.extend_from_slice(&chained.signature);
        nl.push(1);
        let line = alloc::format!("{}\n", certs[0].encode());
        nl.extend_from_slice(&(line.len() as u16).to_le_bytes());
        nl.extend_from_slice(line.as_bytes());
        assert_eq!(parse(&nl), None);
        // Flipping any one byte never panics.
        for at in 0..one.len() {
            let mut b = one.clone();
            b[at] ^= 0xff;
            let _ = parse(&b);
        }
    }

    #[test]
    fn malformed_signature_sections_are_bad_signatures() {
        let key = crate::public_key(&KEY);
        let t = trust(core::slice::from_ref(&key), &[]);
        for body in [&b""[..], b"\x01", b"\x01\x01", b"\x02\x01\x00", b"junk"] {
            let mut m = module();
            m.extend_from_slice(&custom(SECTION, body));
            assert_eq!(verdict(&m, &t), Err(BAD_SIGNATURE), "{body:?}");
            assert_eq!(sign(&m, &KEY, Vec::new()), Err(BAD_SIGNATURE));
        }
        // A signature for another module.
        let other = sign(HEADER, &KEY, Vec::new()).unwrap();
        let mut m = module();
        m.extend_from_slice(&other[HEADER.len()..]);
        assert_eq!(verdict(&m, &t), Err(BAD_SIGNATURE));
        // An ABI too long to sign.
        let mut m = HEADER.to_vec();
        m.extend_from_slice(&custom("purgatorio.abi", &[b'9'; 300]));
        assert_eq!(sign(&m, &KEY, Vec::new()), Err(BAD_SIGNATURE));
        assert_eq!(
            sign(b"not wasm", &KEY, Vec::new()),
            Err(purg_abi::error::EBADEXEC)
        );
    }

    #[test]
    fn standings_are_ranked() {
        let d = digest(&module()).unwrap();
        let msg = message(&d, b"0.6").unwrap();
        let key = crate::public_key(&KEY);
        let e = Entry::sign(&KEY, Vec::new(), &d, b"0.6").unwrap();
        assert_eq!(standing(&e, &msg, &trust(&[key], &[])), Standing::Trusted);
        assert_eq!(standing(&e, &msg, &trust(&[], &[])), Standing::Untrusted);
        assert_eq!(standing(&e, b"other", &trust(&[key], &[])), Standing::Bad);
        assert!(Standing::Trusted > Standing::Revoked);
        assert!(Standing::Revoked > Standing::Expired);
        assert!(Standing::Expired > Standing::Untrusted);
        assert!(Standing::Untrusted > Standing::Bad);
        // An untrusted entry beside a broken one reports the better.
        let mut broken = Entry::sign(&[5; SEED], Vec::new(), &d, b"0.6").unwrap();
        broken.signature[0] ^= 1;
        let mut m = module();
        m.extend_from_slice(&custom(SECTION, &encode(&[broken, e])));
        assert_eq!(verdict(&m, &trust(&[], &[])), Err(UNTRUSTED));
    }
}
