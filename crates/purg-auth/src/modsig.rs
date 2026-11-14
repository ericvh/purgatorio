//! Module digests and signatures (`docs/signing.md`).
//!
//! A module's digest is SHA-256 over its bytes with every `purgatorio.sig`
//! custom section cut out. That section holds one or more entries, each a
//! signature over [`message`] with either the bare signer key or a chain
//! of certificates: an Ed25519 key with `pgcert1` certificates, or a
//! hybrid Ed25519 + ML-DSA-65 key with `pgcert2` ones. [`check`] decides
//! whether a module passes for a set of trust anchors and a revocation
//! list.

use alloc::vec::Vec;
use core::ops::Range;

use crate::sha256::Sha256;

use crate::cert::{AnyCert, Cert, Cert2};
use crate::sig::{HYBRID_PUBLIC, HYBRID_SIGNATURE, Id, Public, Secret};
use crate::{PUBLIC, SIGNATURE};

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
/// A `pgcert1` line is at most this long.
pub const MAX_CERT_LEN: usize = 2048;
/// A `pgcert2` line is at most this long: one with a 255-byte name and
/// all four signatures is 47,157 bytes.
pub const MAX_CERT2_LEN: usize = 48 * 1024;

const KIND_KEY: u8 = 0;
const KIND_CERTS: u8 = 1;
const KIND_HYBRID_KEY: u8 = 2;
const KIND_HYBRID_CERTS: u8 = 3;
const HEADER: &[u8] = b"\0asm\x01\0\0\0";

pub const NOT_SIGNED: &str = "module not signed";
pub const BAD_SIGNATURE: &str = "bad signature";
pub const MODULE_REVOKED: &str = "module revoked";
pub use purg_abi::auth::{EXPIRED, PQSIG_REQUIRED, REVOKED, UNTRUSTED};
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

/// One signature in a `purgatorio.sig` section: by a bare key (kinds 0
/// and 2) or by a key with a chain of certificates (kinds 1 and 3). An
/// entry is of one kind throughout: an Ed25519 key with `pgcert1`
/// certificates, or a hybrid Ed25519 + ML-DSA-65 key with `pgcert2` ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// [`SIGNATURE`] bytes for an Ed25519 key, [`HYBRID_SIGNATURE`] for a
    /// hybrid one.
    pub signature: Vec<u8>,
    key: Public,
    /// The chain: the first certificate is for the signing key, and each
    /// later one is for a key that signed the one before it. Empty for a
    /// bare key.
    certs: Vec<AnyCert>,
}

impl Entry {
    /// An entry around a signature made elsewhere (by an agent). `certs`
    /// is the chain for `key`, or empty for a bare key.
    pub fn new(
        key: Public,
        signature: Vec<u8>,
        certs: Vec<AnyCert>,
    ) -> Result<Entry, &'static str> {
        if signature.len() != key.signature_len() {
            return Err(BAD_SIGNATURE);
        }
        if !certs.is_empty() {
            let max = if key.is_hybrid() {
                MAX_CERT2_LEN
            } else {
                MAX_CERT_LEN
            };
            if certs.len() > MAX_CERTS
                || certs[0].key() != key
                || certs.iter().any(|c| {
                    c.is_hybrid() != key.is_hybrid()
                        || c.encode().len() > max
                        || c.signers().is_empty()
                })
            {
                return Err(EBADCHAIN);
            }
        }
        Ok(Entry {
            signature,
            key,
            certs,
        })
    }

    /// Signs `digest` and `abi` with `secret`. A hybrid key's public half
    /// is taken from its certificate when there is one.
    pub fn sign(
        secret: &Secret,
        certs: Vec<AnyCert>,
        digest: &[u8; DIGEST],
        abi: &[u8],
    ) -> Result<Entry, &'static str> {
        let msg = message(digest, abi).ok_or(BAD_SIGNATURE)?;
        let key = match certs.first() {
            Some(c) if c.is_hybrid() && secret.is_hybrid() => c.key(),
            _ => secret.public(),
        };
        Entry::new(key, secret.sign(&msg), certs)
    }

    /// The key the signature is by.
    pub fn key(&self) -> &Public {
        &self.key
    }

    /// Whether the entry is by a hybrid key.
    pub fn is_hybrid(&self) -> bool {
        self.key.is_hybrid()
    }

    /// The certificates, first the signing key's. Empty for a bare key.
    pub fn certs(&self) -> &[AnyCert] {
        &self.certs
    }

    fn kind(&self) -> u8 {
        match (self.key.is_hybrid(), self.certs.is_empty()) {
            (false, true) => KIND_KEY,
            (false, false) => KIND_CERTS,
            (true, true) => KIND_HYBRID_KEY,
            (true, false) => KIND_HYBRID_CERTS,
        }
    }

    /// The entry as it is stored in the section.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = alloc::vec![self.kind()];
        out.extend_from_slice(&self.signature);
        if self.certs.is_empty() {
            out.extend_from_slice(self.key.bytes());
        } else {
            out.push(self.certs.len() as u8);
            for c in &self.certs {
                let line = c.encode();
                out.extend_from_slice(&(line.len() as u16).to_le_bytes());
                out.extend_from_slice(line.as_bytes());
            }
        }
        out
    }

    /// Parses one entry from the front of `buf`, returning what is left.
    fn take(buf: &[u8]) -> Option<(Entry, &[u8])> {
        let (&kind, rest) = buf.split_first()?;
        let (hybrid, chain) = match kind {
            KIND_KEY => (false, false),
            KIND_CERTS => (false, true),
            KIND_HYBRID_KEY => (true, false),
            KIND_HYBRID_CERTS => (true, true),
            _ => return None,
        };
        let (siglen, keylen, max) = if hybrid {
            (HYBRID_SIGNATURE, HYBRID_PUBLIC, MAX_CERT2_LEN)
        } else {
            (SIGNATURE, PUBLIC, MAX_CERT_LEN)
        };
        let (signature, rest) = (rest.get(..siglen)?, rest.get(siglen..)?);
        if !chain {
            let key = Public::from_slice(rest.get(..keylen)?)?;
            let e = Entry::new(key, signature.to_vec(), Vec::new()).ok()?;
            return Some((e, &rest[keylen..]));
        }
        let (&n, mut rest) = rest.split_first()?;
        if n == 0 || usize::from(n) > MAX_CERTS {
            return None;
        }
        let mut certs = Vec::new();
        for _ in 0..n {
            let (len, r) = rest.split_first_chunk::<2>()?;
            let len = usize::from(u16::from_le_bytes(*len));
            if len == 0 || len > max || len > r.len() {
                return None;
            }
            let (line, r) = r.split_at(len);
            // The parsers allow one trailing newline; the section does
            // not.
            if line.ends_with(b"\n") {
                return None;
            }
            let c = if hybrid {
                AnyCert::V2(Cert2::parse(line).ok()?)
            } else {
                AnyCert::V1(Cert::parse(line).ok()?)
            };
            certs.push(c);
            rest = r;
        }
        let key = certs[0].key();
        let e = Entry::new(key, signature.to_vec(), certs).ok()?;
        Some((e, rest))
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

/// Whether checking the module takes ML-DSA work: its one signature
/// section holds a hybrid entry. The kernel then checks it on a deep
/// stack.
pub fn has_hybrid(info: &Info<'_>) -> bool {
    entries(info).is_ok_and(|e| e.iter().any(Entry::is_hybrid))
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

/// Signs `wasm` with `secret` and returns the signed module.
pub fn sign(wasm: &[u8], secret: &Secret, certs: Vec<AnyCert>) -> Result<Vec<u8>, &'static str> {
    let info = inspect(wasm).map_err(|Malformed| purg_abi::error::EBADEXEC)?;
    let entry = Entry::sign(secret, certs, &info.digest, info.abi)?;
    attach(wasm, entry)
}

/// What a module policy trusts.
pub struct Trust<'a> {
    /// `/lib/keys/modules`: keys that may sign modules or certify signers.
    /// A hybrid key is listed by its id, and matches only hybrid entries.
    pub anchors: &'a [Id],
    /// `/lib/keys/revoked`: keys and module digests (a digest is listed
    /// as 64 bare hex digits, as an Ed25519 key is). `None` when the list
    /// is damaged: nothing then passes.
    pub revoked: Option<&'a [Id]>,
    /// Seconds since 1970 UTC.
    pub now: u64,
    /// Only hybrid entries count: an Ed25519 one that would pass is
    /// refused with [`PQSIG_REQUIRED`].
    pub hybrid: bool,
}

impl Trust<'_> {
    fn revoked(&self, key: &Id) -> bool {
        self.revoked.is_none_or(|r| r.contains(key))
    }

    /// Whether the digest `d` (of a module or an image) is revoked. A
    /// damaged list revokes everything.
    pub fn digest_revoked(&self, d: &[u8; DIGEST]) -> bool {
        self.revoked.is_none_or(|r| r.contains(&Id::Ed25519(*d)))
    }
}

/// How one entry stands. Ordered from worst to best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    /// The signature does not verify.
    Bad,
    /// It verifies, but it is Ed25519 and the policy takes only hybrid
    /// signatures.
    Classical,
    Untrusted,
    Expired,
    Revoked,
    Trusted,
}

impl Standing {
    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Bad => "bad",
            Standing::Classical => "classical",
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
            Standing::Classical => PQSIG_REQUIRED,
            Standing::Untrusted => UNTRUSTED,
            Standing::Expired => EXPIRED,
            Standing::Revoked => REVOKED,
        }
    }
}

/// Judges one entry of a module whose signed message is `msg`. A hybrid
/// entry costs an ML-DSA-65 check for its signature and for each
/// certificate signature it looks at.
pub fn standing(entry: &Entry, msg: &[u8], trust: &Trust<'_>) -> Standing {
    if !entry.key.verify(msg, &entry.signature) {
        return Standing::Bad;
    }
    if trust.hybrid && !entry.is_hybrid() {
        return Standing::Classical;
    }
    let key = entry.key.id();
    if entry.certs.is_empty() {
        return if trust.revoked(&key) {
            Standing::Revoked
        } else if trust.anchors.contains(&key) {
            Standing::Trusted
        } else {
            Standing::Untrusted
        };
    }
    let certs = &entry.certs;
    let ids: Vec<Id> = certs.iter().map(|c| c.key().id()).collect();
    if ids.iter().any(|k| trust.revoked(k)) {
        return Standing::Revoked;
    }
    if trust.anchors.contains(&ids[0]) {
        return Standing::Trusted;
    }
    if certs.iter().any(|c| c.expired(trust.now)) {
        return Standing::Expired;
    }
    let mut revoked_anchor = false;
    for (i, c) in certs.iter().enumerate() {
        for s in c.signers() {
            if trust.anchors.contains(&s) && c.signed_by(&s) {
                if !trust.revoked(&s) {
                    return Standing::Trusted;
                }
                revoked_anchor = true;
            }
        }
        if !ids.get(i + 1).is_some_and(|next| c.signed_by(next)) {
            break;
        }
    }
    if revoked_anchor {
        Standing::Revoked
    } else {
        Standing::Untrusted
    }
}

/// Checks a module against `trust`. Returns the ids of the signing keys
/// of its trusted entries, or the error a `require` policy refuses it
/// with.
pub fn check(info: &Info<'_>, trust: &Trust<'_>) -> Result<Vec<Id>, &'static str> {
    if info.sigs.is_empty() {
        return Err(NOT_SIGNED);
    }
    if trust.revoked.is_some() && trust.digest_revoked(&info.digest) {
        return Err(MODULE_REVOKED);
    }
    let entries = entries(info)?;
    let msg = message(&info.digest, info.abi).ok_or(BAD_SIGNATURE)?;
    let mut best = Standing::Bad;
    let mut signers = Vec::new();
    for e in &entries {
        let s = standing(e, &msg, trust);
        if s == Standing::Trusted {
            signers.push(e.key.id());
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

    use crate::SEED;
    use alloc::boxed::Box;

    /// A list of Ed25519 keys as a trust list holds them.
    fn ids(keys: &[[u8; PUBLIC]]) -> &'static [Id] {
        Box::leak(
            keys.iter()
                .map(Id::from)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    fn trust(anchors: &[[u8; PUBLIC]], revoked: &[[u8; PUBLIC]]) -> Trust<'static> {
        Trust {
            anchors: ids(anchors),
            revoked: Some(ids(revoked)),
            now: 1000,
            hybrid: false,
        }
    }

    /// Signs with an Ed25519 seed and a `pgcert1` chain.
    fn sign(wasm: &[u8], seed: &[u8; SEED], certs: Vec<Cert>) -> Result<Vec<u8>, &'static str> {
        super::sign(
            wasm,
            &Secret::Ed25519(*seed),
            certs.into_iter().map(AnyCert::V1).collect(),
        )
    }

    fn esign(
        seed: &[u8; SEED],
        certs: Vec<Cert>,
        digest: &[u8; DIGEST],
        abi: &[u8],
    ) -> Result<Entry, &'static str> {
        Entry::sign(
            &Secret::Ed25519(*seed),
            certs.into_iter().map(AnyCert::V1).collect(),
            digest,
            abi,
        )
    }

    fn cert(name: &str, key: &[u8; SEED], expires: u64, by: &[u8; SEED]) -> Cert {
        let mut c = Cert::new(name, crate::public_key(key), expires).unwrap();
        c.sign(by).unwrap();
        c
    }

    fn verdict(wasm: &[u8], t: &Trust<'_>) -> Result<Vec<[u8; PUBLIC]>, &'static str> {
        check(&inspect(wasm).unwrap(), t).map(|v| {
            v.iter()
                .map(|i| match i {
                    Id::Ed25519(k) | Id::Hybrid(k) => *k,
                })
                .collect()
        })
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
            anchors: ids(&[key]),
            revoked: None,
            now: 0,
            hybrid: false,
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
                assert_ne!(
                    check(&info, &t),
                    Ok(alloc::vec![Id::from(key)]),
                    "byte {at}"
                );
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
            anchors: ids(&[root]),
            revoked: Some(&[]),
            now: 2000,
            hybrid: false,
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
        let bare = esign(&KEY, Vec::new(), &d, b"0.6").unwrap();
        let chained = esign(
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
        let certs = chained.certs();
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
        let e = esign(&KEY, Vec::new(), &d, b"0.6").unwrap();
        assert_eq!(standing(&e, &msg, &trust(&[key], &[])), Standing::Trusted);
        assert_eq!(standing(&e, &msg, &trust(&[], &[])), Standing::Untrusted);
        assert_eq!(standing(&e, b"other", &trust(&[key], &[])), Standing::Bad);
        assert!(Standing::Trusted > Standing::Revoked);
        assert!(Standing::Revoked > Standing::Expired);
        assert!(Standing::Expired > Standing::Untrusted);
        assert!(Standing::Untrusted > Standing::Bad);
        // An untrusted entry beside a broken one reports the better.
        let mut broken = esign(&[5; SEED], Vec::new(), &d, b"0.6").unwrap();
        broken.signature[0] ^= 1;
        let mut m = module();
        m.extend_from_slice(&custom(SECTION, &encode(&[broken, e])));
        assert_eq!(verdict(&m, &trust(&[], &[])), Err(UNTRUSTED));
    }

    fn hybrid(b: u8) -> Secret {
        let mut s = [0u8; crate::sig::HYBRID_SEED];
        for (i, x) in s.iter_mut().enumerate() {
            *x = b.wrapping_add(i as u8);
        }
        Secret::Hybrid(crate::sig::HybridSecret(s))
    }

    fn htrust(anchors: &[Id], hybrid: bool) -> Trust<'static> {
        Trust {
            anchors: Box::leak(anchors.to_vec().into_boxed_slice()),
            revoked: Some(&[]),
            now: 1000,
            hybrid,
        }
    }

    fn hcheck(wasm: &[u8], t: &Trust<'_>) -> Result<Vec<Id>, &'static str> {
        check(&inspect(wasm).unwrap(), t)
    }

    fn hcert(name: &str, key: &Secret, expires: u64, by: &Secret) -> AnyCert {
        let Public::Hybrid(k) = key.public() else {
            unreachable!()
        };
        let mut c = AnyCert::V2(Cert2::new(name, k, expires).unwrap());
        c.sign(by).unwrap();
        c
    }

    /// A hybrid entry: its format, a fixed vector, and that only a listed
    /// hybrid id trusts it.
    #[test]
    fn hybrid_bare_key() {
        let k = hybrid(1);
        let id = k.public().id();
        let signed = super::sign(&module(), &k, Vec::new()).unwrap();
        // Deterministic, as Ed25519 is.
        assert_eq!(super::sign(&module(), &k, Vec::new()).unwrap(), signed);
        let body = inspect(&signed).unwrap().sigs[0];
        assert_eq!(body.len(), 2 + 1 + HYBRID_SIGNATURE + HYBRID_PUBLIC);
        assert_eq!(&body[..3], &[VERSION, 1, KIND_HYBRID_KEY]);
        assert_eq!(&body[3 + HYBRID_SIGNATURE..], k.public().bytes());
        let sha: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(body).into();
        assert_eq!(
            hex::encode(&sha),
            "54e4869ac656e29875bc73a90ecf6086099d4bcddda88fdae4142d16ed9c0305"
        );
        assert_eq!(hcheck(&signed, &htrust(&[id], false)), Ok(alloc::vec![id]));
        assert_eq!(hcheck(&signed, &htrust(&[id], true)), Ok(alloc::vec![id]));
        assert_eq!(hcheck(&signed, &htrust(&[], false)), Err(UNTRUSTED));
        // The id without its marker is an Ed25519 entry: it trusts
        // nothing here.
        let Id::Hybrid(raw) = id else { unreachable!() };
        assert_eq!(
            hcheck(&signed, &htrust(&[Id::Ed25519(raw)], false)),
            Err(UNTRUSTED)
        );
        // Revoked by its id.
        let t = Trust {
            anchors: Box::leak(alloc::vec![id].into_boxed_slice()),
            revoked: Some(Box::leak(alloc::vec![id].into_boxed_slice())),
            now: 0,
            hybrid: false,
        };
        assert_eq!(hcheck(&signed, &t), Err(REVOKED));
        // Every truncation of the section is refused.
        for n in 0..body.len() {
            assert_eq!(parse(&body[..n]), None, "length {n}");
        }
    }

    /// Cutting the ML-DSA half off leaves an Ed25519 signature over the
    /// framed message, which is no module signature.
    #[test]
    fn a_stripped_hybrid_entry_does_not_verify() {
        let k = hybrid(1);
        let signed = super::sign(&module(), &k, Vec::new()).unwrap();
        let body = inspect(&signed).unwrap().sigs[0];
        let ed = &body[3..3 + SIGNATURE];
        let kp = k.public();
        let edkey = &kp.bytes()[..PUBLIC];
        let mut cut = alloc::vec![VERSION, 1, KIND_KEY];
        cut.extend_from_slice(ed);
        cut.extend_from_slice(edkey);
        let mut m = module();
        m.extend_from_slice(&custom(SECTION, &cut));
        let edid = Id::Ed25519(edkey.try_into().unwrap());
        assert_eq!(hcheck(&m, &htrust(&[edid], false)), Err(BAD_SIGNATURE));
        // A hybrid kind with an Ed25519 signature's length is malformed.
        let mut wrong = alloc::vec![VERSION, 1, KIND_HYBRID_KEY];
        wrong.extend_from_slice(ed);
        wrong.extend_from_slice(k.public().bytes());
        assert_eq!(parse(&wrong), None);
    }

    #[test]
    fn hybrid_certificate_chains() {
        let key = hybrid(1);
        let mid = hybrid(2);
        let root = hybrid(3);
        let rootid = root.public().id();
        let chain = alloc::vec![
            hcert("builder", &key, 2000, &mid),
            hcert("release", &mid, 0, &root),
        ];
        let signed = super::sign(&module(), &key, chain.clone()).unwrap();
        let body = inspect(&signed).unwrap().sigs[0];
        assert_eq!(body[2], KIND_HYBRID_CERTS);
        let want = Ok(alloc::vec![key.public().id()]);
        assert_eq!(hcheck(&signed, &htrust(&[rootid], false)), want);
        assert_eq!(hcheck(&signed, &htrust(&[rootid], true)), want);
        let late = Trust {
            anchors: Box::leak(alloc::vec![rootid].into_boxed_slice()),
            revoked: Some(&[]),
            now: 2000,
            hybrid: false,
        };
        assert_eq!(hcheck(&signed, &late), Err(EXPIRED));
        // A chain of one kind only: a pgcert1 for a hybrid key, or a
        // hybrid certificate for an Ed25519 key, does not go in.
        let ed = cert("builder", &KEY, 0, &ROOT);
        assert_eq!(
            super::sign(&module(), &key, alloc::vec![AnyCert::V1(ed)]),
            Err(EBADCHAIN)
        );
        assert_eq!(
            super::sign(&module(), &Secret::Ed25519(KEY), chain),
            Err(EBADCHAIN)
        );
        for n in (0..body.len()).step_by(97) {
            assert_eq!(parse(&body[..n]), None, "length {n}");
        }
    }

    /// A module signed both ways passes an Ed25519 policy on its Ed25519
    /// entry and a hybrid one on its hybrid entry; one signed only with
    /// Ed25519 fails a hybrid policy with its own error.
    #[test]
    fn a_policy_may_require_hybrid() {
        let k = hybrid(1);
        let hid = k.public().id();
        let eid = Id::from(crate::public_key(&KEY));
        let ed = sign(&module(), &KEY, Vec::new()).unwrap();
        assert_eq!(hcheck(&ed, &htrust(&[eid], true)), Err(PQSIG_REQUIRED));
        assert_eq!(hcheck(&ed, &htrust(&[eid, hid], true)), Err(PQSIG_REQUIRED));
        let both = super::sign(&ed, &k, Vec::new()).unwrap();
        assert_eq!(hcheck(&both, &htrust(&[eid], false)), Ok(alloc::vec![eid]));
        assert_eq!(hcheck(&both, &htrust(&[hid], true)), Ok(alloc::vec![hid]));
        assert_eq!(
            hcheck(&both, &htrust(&[eid, hid], false)),
            Ok(alloc::vec![eid, hid])
        );
        // A trusted Ed25519 entry beside an untrusted hybrid one: the
        // hybrid one's standing is the error.
        assert_eq!(hcheck(&both, &htrust(&[eid], true)), Err(UNTRUSTED));
        assert!(Standing::Untrusted > Standing::Classical);
        assert!(Standing::Classical > Standing::Bad);
        let e = Entry::sign(&k, Vec::new(), &[0; 32], b"0.6").unwrap();
        assert!(Entry::parse(&e.encode()).is_some_and(|e| e.is_hybrid()));
    }
}
