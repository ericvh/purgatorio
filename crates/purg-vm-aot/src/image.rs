//! Where images come from: a set linked into the kernel at build time
//! ([`Images`]), or signed image files read at run time ([`Signed`]).
//!
//! A signed image (`<cmd>.cwasm` beside `<cmd>.wasm`) is:
//!
//! ```text
//! magic   "PURGIMG1"          8 bytes
//! kind    u8                  0 native, 1 Pulley
//! zero    [u8; 3]
//! module  [u8; 32]            the module's digest (docs/signing.md)
//! len     u64 LE              the image's length
//! image   [u8; len]           wasmtime's precompiled module
//! sig     ...                 to the end: a `purgatorio.sig` section body
//! ```
//!
//! The signature entries are those of `purgatorio.sig` (a bare key or a
//! certificate chain), over [`message`]: the kind, the module's digest
//! and the image's SHA-256 digest. So an entry says "this native code is
//! a compilation of that module, for this target". The loader checks it
//! against `/lib/keys/aot` and `/lib/keys/revoked` in the boot namespace,
//! and runs the image only for a prog whose module has that digest.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_auth::modsig::{self, DIGEST, Entry, Trust};
use purg_auth::{PUBLIC, SEED};
use sha2::{Digest as _, Sha256};

/// What images hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// aarch64 machine code.
    Native,
    /// Pulley bytecode for a 64-bit little-endian host.
    Pulley,
}

impl Kind {
    pub fn target(self) -> &'static str {
        match self {
            Kind::Native => "aarch64-unknown-none",
            Kind::Pulley => "pulley64",
        }
    }

    fn byte(self) -> u8 {
        match self {
            Kind::Native => 0,
            Kind::Pulley => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Kind> {
        match b {
            0 => Some(Kind::Native),
            1 => Some(Kind::Pulley),
            _ => None,
        }
    }
}

/// Images are aligned to this within a linked set, and the set itself in
/// memory, which covers any page size the code was laid out for.
pub const ALIGN: usize = 64 << 10;

/// The image set's header: `PURGAOT1`, the kind (0 native, 1 Pulley), and
/// the number of entries, then per entry the module's digest and the
/// image's offset and length, all little-endian.
pub const MAGIC: &[u8; 8] = b"PURGAOT1";
const HEADER: usize = 16;
const ENTRY: usize = 48;

/// Images by module digest.
pub(crate) type ByDigest = BTreeMap<[u8; DIGEST], &'static [u8]>;

/// A set of precompiled images linked into the kernel, by module digest.
/// They are trusted as the kernel is: they are part of its image.
pub struct Images {
    pub(crate) kind: Kind,
    pub(crate) by_digest: Arc<ByDigest>,
    /// Where the set is.
    pub(crate) range: core::ops::Range<usize>,
}

impl Images {
    /// Reads a set built by [`Images::build`]. `blob` must start at an
    /// [`ALIGN`] boundary and, for native images, be executable.
    pub fn parse(blob: &'static [u8]) -> Result<Images, String> {
        let bad = || String::from("bad AOT image set");
        if blob.get(..8) != Some(MAGIC.as_slice()) {
            return Err(bad());
        }
        if !(blob.as_ptr() as usize).is_multiple_of(ALIGN) {
            return Err(String::from("AOT image set is not aligned"));
        }
        let u32_at = |i: usize| -> Result<u32, String> {
            Ok(u32::from_le_bytes(
                blob.get(i..i + 4)
                    .ok_or_else(bad)?
                    .try_into()
                    .map_err(|_| bad())?,
            ))
        };
        let u64_at = |i: usize| -> Result<usize, String> {
            let v = u64::from_le_bytes(
                blob.get(i..i + 8)
                    .ok_or_else(bad)?
                    .try_into()
                    .map_err(|_| bad())?,
            );
            usize::try_from(v).map_err(|_| bad())
        };
        let kind = u8::try_from(u32_at(8)?)
            .ok()
            .and_then(Kind::from_byte)
            .ok_or_else(bad)?;
        let n = u32_at(12)? as usize;
        let mut by_digest = BTreeMap::new();
        for i in 0..n {
            let e = HEADER + i * ENTRY;
            let digest: [u8; 32] = blob
                .get(e..e + 32)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?;
            let (off, len) = (u64_at(e + 32)?, u64_at(e + 40)?);
            let image = blob.get(off..off.checked_add(len).ok_or_else(bad)?);
            by_digest.insert(digest, image.ok_or_else(bad)?);
        }
        let base = blob.as_ptr() as usize;
        Ok(Images {
            kind,
            by_digest: Arc::new(by_digest),
            range: base..base + blob.len(),
        })
    }

    /// Lays out a set: `images` are (digest, image) pairs.
    pub fn build(kind: Kind, images: &[([u8; 32], Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&u32::from(kind.byte()).to_le_bytes());
        out.extend_from_slice(&(images.len() as u32).to_le_bytes());
        let table = out.len();
        out.resize(table + images.len() * ENTRY, 0);
        for (i, (digest, image)) in images.iter().enumerate() {
            out.resize(out.len().next_multiple_of(ALIGN), 0);
            let off = out.len();
            out.extend_from_slice(image);
            let e = table + i * ENTRY;
            out[e..e + 32].copy_from_slice(digest);
            out[e + 32..e + 40].copy_from_slice(&(off as u64).to_le_bytes());
            out[e + 40..e + 48].copy_from_slice(&(image.len() as u64).to_le_bytes());
        }
        out
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.by_digest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_digest.is_empty()
    }
}

/// The first bytes of a signed image file.
pub const SIGNED_MAGIC: &[u8; 8] = b"PURGIMG1";
/// The first bytes of every message an image signature signs.
pub const CONTEXT: &[u8] = b"pgaotimg1\0";
const SIGNED_HEADER: usize = 8 + 4 + DIGEST + 8;

/// An image with no signature that verifies against the trust lists.
pub const UNTRUSTED_IMAGE: &str = "untrusted AOT image";
/// An image file that does not parse, or is for another module or kind.
pub const BAD_IMAGE: &str = "bad AOT image";

/// The bytes an image signature signs: the context, the kind, the
/// module's digest and the image's.
pub fn message(kind: Kind, module: &[u8; DIGEST], image: &[u8; DIGEST]) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTEXT.len() + 1 + 2 * DIGEST);
    m.extend_from_slice(CONTEXT);
    m.push(kind.byte());
    m.extend_from_slice(module);
    m.extend_from_slice(image);
    m
}

/// A signed image file, parsed but not verified.
pub struct Signed<'a> {
    pub kind: Kind,
    /// The digest of the module it was compiled from.
    pub module: [u8; DIGEST],
    pub image: &'a [u8],
    entries: Vec<Entry>,
}

impl<'a> Signed<'a> {
    /// Parses a signed image file, strictly: the signature part is a
    /// whole `purgatorio.sig` body with at least one entry.
    pub fn parse(file: &'a [u8]) -> Result<Signed<'a>, &'static str> {
        if file.get(..8) != Some(SIGNED_MAGIC.as_slice()) || file.len() < SIGNED_HEADER {
            return Err(BAD_IMAGE);
        }
        let kind = Kind::from_byte(file[8]).ok_or(BAD_IMAGE)?;
        if file[9..12] != [0, 0, 0] {
            return Err(BAD_IMAGE);
        }
        let module: [u8; DIGEST] = file[12..12 + DIGEST].try_into().map_err(|_| BAD_IMAGE)?;
        let len = u64::from_le_bytes(
            file[12 + DIGEST..SIGNED_HEADER]
                .try_into()
                .map_err(|_| BAD_IMAGE)?,
        );
        let end = usize::try_from(len)
            .ok()
            .and_then(|l| SIGNED_HEADER.checked_add(l))
            .filter(|&e| e <= file.len())
            .ok_or(BAD_IMAGE)?;
        let entries = modsig::parse(&file[end..]).ok_or(BAD_IMAGE)?;
        Ok(Signed {
            kind,
            module,
            image: &file[SIGNED_HEADER..end],
            entries,
        })
    }

    /// The image, if it is a compilation of the module whose digest is
    /// `module`, for `kind`, and one of its signatures is trusted. Neither
    /// the module's digest nor the image's may be revoked.
    pub fn verify(
        &self,
        kind: Kind,
        module: &[u8; DIGEST],
        trust: &Trust<'_>,
    ) -> Result<&'a [u8], &'static str> {
        if self.kind != kind || &self.module != module {
            return Err(BAD_IMAGE);
        }
        let digest: [u8; DIGEST] = Sha256::digest(self.image).into();
        if trust
            .revoked
            .is_none_or(|r| r.contains(module) || r.contains(&digest))
        {
            return Err(modsig::MODULE_REVOKED);
        }
        let msg = message(kind, module, &digest);
        if self
            .entries
            .iter()
            .any(|e| modsig::standing(e, &msg, trust) == modsig::Standing::Trusted)
        {
            Ok(self.image)
        } else {
            Err(UNTRUSTED_IMAGE)
        }
    }

    /// Writes a signed image file for `image`, compiled from the module
    /// whose digest is `module`, signed by `seed` (with `certs`, its
    /// chain, or none for a bare key). Ed25519 is deterministic, so the
    /// same inputs give the same file on any host.
    pub fn build(
        kind: Kind,
        module: &[u8; DIGEST],
        image: &[u8],
        seed: &[u8; SEED],
        certs: Vec<purg_auth::cert::Cert>,
    ) -> Result<Vec<u8>, &'static str> {
        let digest: [u8; DIGEST] = Sha256::digest(image).into();
        let msg = message(kind, module, &digest);
        let key: [u8; PUBLIC] = purg_auth::public_key(seed);
        let entry = Entry::new(key, purg_auth::sign(seed, &msg), certs)?;
        let mut out = Vec::with_capacity(SIGNED_HEADER + image.len() + 128);
        out.extend_from_slice(SIGNED_MAGIC);
        out.extend_from_slice(&[kind.byte(), 0, 0, 0]);
        out.extend_from_slice(module);
        out.extend_from_slice(&(image.len() as u64).to_le_bytes());
        out.extend_from_slice(image);
        out.extend_from_slice(&modsig::encode(&[entry]));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_A: [u8; SEED] = [7; SEED];
    const SEED_B: [u8; SEED] = [9; SEED];

    fn trust<'a>(anchors: &'a [[u8; PUBLIC]], revoked: &'a [[u8; PUBLIC]]) -> Trust<'a> {
        Trust {
            anchors,
            revoked: Some(revoked),
            now: 0,
        }
    }

    #[test]
    fn signed_images() {
        let module = [0x11; DIGEST];
        let image = b"not really code, but bytes all the same";
        let file = Signed::build(Kind::Native, &module, image, &SEED_A, Vec::new()).unwrap();
        let a = purg_auth::public_key(&SEED_A);
        let b = purg_auth::public_key(&SEED_B);
        let s = Signed::parse(&file).unwrap();
        assert_eq!(
            s.verify(Kind::Native, &module, &trust(&[a], &[])),
            Ok(&image[..])
        );
        // Another signer, another module, another kind.
        assert_eq!(
            s.verify(Kind::Native, &module, &trust(&[b], &[])),
            Err(UNTRUSTED_IMAGE)
        );
        assert_eq!(
            s.verify(Kind::Native, &[0x12; DIGEST], &trust(&[a], &[])),
            Err(BAD_IMAGE)
        );
        assert_eq!(
            s.verify(Kind::Pulley, &module, &trust(&[a], &[])),
            Err(BAD_IMAGE)
        );
        // Revoked: the key, the module, the image; or a damaged list.
        let image_digest: [u8; DIGEST] = Sha256::digest(image).into();
        for r in [a, module, image_digest] {
            assert!(s.verify(Kind::Native, &module, &trust(&[a], &[r])).is_err());
        }
        let damaged = Trust {
            anchors: &[a],
            revoked: None,
            now: 0,
        };
        assert!(s.verify(Kind::Native, &module, &damaged).is_err());
        // A changed byte anywhere in the image breaks the signature, and
        // a truncated file does not parse.
        for i in [SIGNED_HEADER, SIGNED_HEADER + image.len() - 1] {
            let mut f = file.clone();
            f[i] ^= 1;
            let s = Signed::parse(&f).unwrap();
            assert_eq!(
                s.verify(Kind::Native, &module, &trust(&[a], &[])),
                Err(UNTRUSTED_IMAGE)
            );
        }
        for n in 0..file.len() {
            assert!(Signed::parse(&file[..n]).is_err(), "{n}");
        }
        // The same inputs give the same bytes.
        assert_eq!(
            file,
            Signed::build(Kind::Native, &module, image, &SEED_A, Vec::new()).unwrap()
        );
    }

    #[test]
    fn linked_sets() {
        let set = Images::build(Kind::Pulley, &[([1; 32], alloc::vec![5; 10])]);
        // `parse` wants a 'static, aligned blob.
        let leaked: &'static mut [u8] = alloc::vec![0u8; set.len() + ALIGN].leak();
        let off = leaked.as_ptr().align_offset(ALIGN);
        leaked[off..off + set.len()].copy_from_slice(&set);
        let images = Images::parse(&leaked[off..off + set.len()]).unwrap();
        assert_eq!(images.kind(), Kind::Pulley);
        assert_eq!(
            images.by_digest.get(&[1; 32]).copied(),
            Some(&[5u8; 10][..])
        );
    }
}
