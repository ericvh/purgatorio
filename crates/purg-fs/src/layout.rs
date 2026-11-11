//! The on-disk format: the superblock, directory entries and the journal
//! header. Every field is little-endian and decoded with `from_le_bytes`
//! from byte slices; nothing is cast. `docs/fs.md` has the tables.

use alloc::borrow::Cow;
use alloc::string::String;

use crate::crc::{Fnv, crc32c};
use crate::{Error, Result};

/// The superblock's magic number, at byte 0 of block 0.
pub const MAGIC: [u8; 8] = *b"purgfs\n1";
/// The format version.
pub const VERSION: u32 = 1;
/// The journal header's magic number.
pub const JMAGIC: [u8; 8] = *b"purgjnl1";

/// The smallest and largest block size.
pub const MIN_BSIZE: usize = 512;
pub const MAX_BSIZE: usize = 64 << 10;
/// The default block size.
pub const BSIZE: usize = 4096;
/// The smallest and default journal, in blocks (one header and the
/// images).
pub const MIN_JBLOCKS: u32 = 8;
pub const JBLOCKS: u32 = 32;

/// Bytes of the superblock that hold its fields (the root's entry follows
/// at [`ROOT_OFF`]).
pub const SUPER_LEN: usize = 256;
/// Where the root directory's entry is in block 0.
pub const ROOT_OFF: usize = 256;
/// Where the superblock's checksum is: over bytes `0..252` and the root
/// entry.
const SUPER_CRC: usize = 252;

/// The size of a directory entry.
pub const DENTRY: usize = 256;
/// Direct block pointers in an entry; then one single and one double
/// indirect pointer.
pub const NDIRECT: usize = 16;
pub const NPTR: usize = NDIRECT + 2;
/// The longest name, and the longest user or group name, in bytes.
pub const NAME_MAX: usize = 64;
pub const USER_MAX: usize = 24;

/// Inferno's mode bits.
pub const DMDIR: u32 = 0x8000_0000;
pub const DMAPPEND: u32 = 0x4000_0000;
pub const DMEXCL: u32 = 0x2000_0000;
/// The mode bits a file may have.
pub const MODE_MASK: u32 = DMDIR | DMAPPEND | DMEXCL | 0o777;

fn corrupt(what: &'static str) -> Error {
    Cow::Borrowed(what)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// A pointer in an indirect block.
pub fn ptr_at(b: &[u8], i: usize) -> u32 {
    u32_at(b, 4 * i)
}

pub fn set_ptr(b: &mut [u8], i: usize, v: u32) {
    put32(b, 4 * i, v)
}

/// The superblock's fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Super {
    pub bsize: u32,
    /// Blocks in the file system, block 0 included.
    pub nblocks: u64,
    /// The journal: `jlen` blocks from `jstart` (always 1).
    pub jstart: u64,
    pub jlen: u32,
    /// The free map: `blen` blocks from `bstart`, one bit per block.
    pub bstart: u64,
    pub blen: u32,
    /// The first data block; blocks before it are the superblock, the
    /// journal and the free map.
    pub dstart: u64,
    /// The next qid path.
    pub qidgen: u64,
}

impl Super {
    /// The layout of a file system of `nblocks` blocks of `bsize` bytes
    /// with a journal of `jlen` blocks.
    pub fn layout(bsize: usize, nblocks: u64, jlen: u32) -> Result<Super> {
        if !bsize.is_power_of_two() || !(MIN_BSIZE..=MAX_BSIZE).contains(&bsize) {
            return Err(corrupt(
                "block size must be a power of two from 512 to 65536",
            ));
        }
        if jlen < MIN_JBLOCKS {
            return Err(corrupt("journal too small"));
        }
        let per = 8 * bsize as u64;
        let blen = nblocks.div_ceil(per);
        let bstart = 1 + u64::from(jlen);
        let dstart = bstart + blen;
        if nblocks > u64::from(u32::MAX) || blen > u64::from(u32::MAX) {
            return Err(corrupt("file system too large"));
        }
        // Room for the root's first directory block and a little more.
        if dstart + 4 > nblocks {
            return Err(corrupt("file system too small"));
        }
        Ok(Super {
            bsize: bsize as u32,
            nblocks,
            jstart: 1,
            jlen,
            bstart,
            blen: blen as u32,
            dstart,
            qidgen: 1,
        })
    }

    /// Checks the fields against each other and against a device of `size`
    /// bytes in units of `sector`.
    pub fn validate(&self, size: u64, sector: usize) -> Result<()> {
        let want = Super::layout(self.bsize as usize, self.nblocks, self.jlen)
            .map_err(|_| corrupt("bad superblock"))?;
        if self.jstart != want.jstart
            || self.bstart != want.bstart
            || self.blen != want.blen
            || self.dstart != want.dstart
        {
            return Err(corrupt("bad superblock"));
        }
        if sector == 0 || !(self.bsize as usize).is_multiple_of(sector) {
            return Err(corrupt("block size is not a multiple of the device's"));
        }
        if self.nblocks.saturating_mul(u64::from(self.bsize)) > size {
            return Err(corrupt("file system larger than the device"));
        }
        if self.qidgen == 0 {
            return Err(corrupt("bad superblock"));
        }
        Ok(())
    }

    /// Writes the fields into block 0 (`b`, at least 512 bytes), leaving
    /// the root's entry; [`seal_super`] then sets the CRC.
    pub fn encode(&self, b: &mut [u8]) {
        b[..SUPER_LEN].fill(0);
        b[0..8].copy_from_slice(&MAGIC);
        put32(b, 8, VERSION);
        put32(b, 12, self.bsize);
        put64(b, 16, self.nblocks);
        put64(b, 24, self.jstart);
        put32(b, 32, self.jlen);
        put32(b, 36, self.blen);
        put64(b, 40, self.bstart);
        put64(b, 48, self.dstart);
        put64(b, 56, self.qidgen);
    }

    /// Reads the fields from block 0 (at least 512 bytes of it), checking
    /// the magic number, the version and the CRC.
    pub fn decode(b: &[u8]) -> Result<Super> {
        if b.len() < MIN_BSIZE || b[0..8] != MAGIC {
            return Err(corrupt("not a purgfs file system"));
        }
        if u32_at(b, 8) != VERSION {
            return Err(corrupt("unknown purgfs version"));
        }
        if u32_at(b, SUPER_CRC) != super_crc(b) {
            return Err(corrupt("bad superblock checksum"));
        }
        Ok(Super {
            bsize: u32_at(b, 12),
            nblocks: u64_at(b, 16),
            jstart: u64_at(b, 24),
            jlen: u32_at(b, 32),
            blen: u32_at(b, 36),
            bstart: u64_at(b, 40),
            dstart: u64_at(b, 48),
            qidgen: u64_at(b, 56),
        })
    }
}

fn super_crc(b: &[u8]) -> u32 {
    Fnv::new()
        .update(&b[..SUPER_CRC])
        .update(&b[ROOT_OFF..ROOT_OFF + DENTRY])
        .finish()
}

/// Sets block 0's CRC after its fields or the root's entry changed.
pub fn seal_super(b: &mut [u8]) {
    let crc = super_crc(b);
    put32(b, SUPER_CRC, crc);
}

/// A directory entry: the file's name, owner, mode, times, qid, length
/// and block pointers. There are no inodes: the entry in its parent's
/// directory block is the file (as in Plan 9's kfs), and the root's is in
/// block 0.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Dentry {
    pub name: String,
    pub uid: String,
    pub gid: String,
    pub muid: String,
    pub qpath: u64,
    pub qvers: u32,
    pub mode: u32,
    pub atime: u32,
    pub mtime: u32,
    /// Bytes, for a file; for a directory, its blocks times the block
    /// size (a stat reports 0).
    pub length: u64,
    /// [`NDIRECT`] direct pointers, then the single and the double
    /// indirect pointer. 0 is no block (a hole).
    pub ptrs: [u32; NPTR],
}

/// What a directory slot holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Slot {
    Free,
    Used(Dentry),
    /// A used slot that fails its CRC or its field checks.
    Bad(&'static str),
}

// Entry offsets.
const D_USED: usize = 0;
const D_NAMELEN: usize = 1;
const D_UIDLEN: usize = 2;
const D_GIDLEN: usize = 3;
const D_MUIDLEN: usize = 4;
const D_QPATH: usize = 8;
const D_QVERS: usize = 16;
const D_MODE: usize = 20;
const D_ATIME: usize = 24;
const D_MTIME: usize = 28;
const D_LENGTH: usize = 32;
const D_PTRS: usize = 40;
const D_NAME: usize = 116;
const D_UID: usize = D_NAME + NAME_MAX;
const D_GID: usize = D_UID + USER_MAX;
const D_MUID: usize = D_GID + USER_MAX;
const D_CRC: usize = 252;

impl Dentry {
    pub fn is_dir(&self) -> bool {
        self.mode & DMDIR != 0
    }

    /// Writes the entry into `b` (exactly [`DENTRY`] bytes). The names
    /// must fit ([`valid_name`], [`valid_user`]).
    pub fn encode(&self, b: &mut [u8]) {
        b[..DENTRY].fill(0);
        b[D_USED] = 1;
        b[D_NAMELEN] = self.name.len() as u8;
        b[D_UIDLEN] = self.uid.len() as u8;
        b[D_GIDLEN] = self.gid.len() as u8;
        b[D_MUIDLEN] = self.muid.len() as u8;
        put64(b, D_QPATH, self.qpath);
        put32(b, D_QVERS, self.qvers);
        put32(b, D_MODE, self.mode);
        put32(b, D_ATIME, self.atime);
        put32(b, D_MTIME, self.mtime);
        put64(b, D_LENGTH, self.length);
        for (i, p) in self.ptrs.iter().enumerate() {
            put32(b, D_PTRS + 4 * i, *p);
        }
        b[D_NAME..D_NAME + self.name.len()].copy_from_slice(self.name.as_bytes());
        b[D_UID..D_UID + self.uid.len()].copy_from_slice(self.uid.as_bytes());
        b[D_GID..D_GID + self.gid.len()].copy_from_slice(self.gid.as_bytes());
        b[D_MUID..D_MUID + self.muid.len()].copy_from_slice(self.muid.as_bytes());
        let crc = crc32c(&b[..D_CRC]);
        put32(b, D_CRC, crc);
    }

    /// Reads a slot ([`DENTRY`] bytes of `b`).
    pub fn decode(b: &[u8]) -> Slot {
        if b.len() < DENTRY {
            return Slot::Bad("short entry");
        }
        if b[D_USED] == 0 {
            return Slot::Free;
        }
        if b[D_USED] != 1 {
            return Slot::Bad("bad entry flag");
        }
        if u32_at(b, D_CRC) != crc32c(&b[..D_CRC]) {
            return Slot::Bad("bad entry checksum");
        }
        let text = |at: usize, len: u8, max: usize| -> Option<String> {
            let len = len as usize;
            if len > max {
                return None;
            }
            core::str::from_utf8(&b[at..at + len])
                .ok()
                .map(String::from)
        };
        let (Some(name), Some(uid), Some(gid), Some(muid)) = (
            text(D_NAME, b[D_NAMELEN], NAME_MAX),
            text(D_UID, b[D_UIDLEN], USER_MAX),
            text(D_GID, b[D_GIDLEN], USER_MAX),
            text(D_MUID, b[D_MUIDLEN], USER_MAX),
        ) else {
            return Slot::Bad("bad entry name");
        };
        let mode = u32_at(b, D_MODE);
        if mode & !MODE_MASK != 0 {
            return Slot::Bad("bad entry mode");
        }
        let mut ptrs = [0u32; NPTR];
        for (i, p) in ptrs.iter_mut().enumerate() {
            *p = u32_at(b, D_PTRS + 4 * i);
        }
        Slot::Used(Dentry {
            name,
            uid,
            gid,
            muid,
            qpath: u64_at(b, D_QPATH),
            qvers: u32_at(b, D_QVERS),
            mode,
            atime: u32_at(b, D_ATIME),
            mtime: u32_at(b, D_MTIME),
            length: u64_at(b, D_LENGTH),
            ptrs,
        })
    }
}

/// Whether `name` can be a file's name: 1 to [`NAME_MAX`] bytes, not `.`
/// or `..`, without `/` or NUL.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != "."
        && name != ".."
        && !name.bytes().any(|c| c == b'/' || c == 0)
}

/// Whether `user` can be an owner or group: 1 to [`USER_MAX`] bytes,
/// without NUL.
pub fn valid_user(user: &str) -> bool {
    !user.is_empty() && user.len() <= USER_MAX && !user.bytes().any(|c| c == 0)
}

/// The journal header, in the journal's first block: the transaction's
/// sequence number, its block count, a CRC over the header, the list of
/// home blocks and the images, then the home block number of each image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JHeader {
    pub seq: u64,
    pub crc: u32,
    pub homes: alloc::vec::Vec<u64>,
}

const J_SEQ: usize = 8;
const J_COUNT: usize = 16;
const J_CRC: usize = 20;
const J_HOMES: usize = 24;

impl JHeader {
    /// The most images a header of `bsize` bytes and a journal of `jlen`
    /// blocks can list.
    pub fn capacity(bsize: usize, jlen: u32) -> usize {
        ((bsize - J_HOMES) / 8).min(jlen as usize - 1)
    }

    /// Writes the header for `homes` into `b` (one block) with the CRC of
    /// it and `images`.
    pub fn encode(seq: u64, homes: &[u64], images: &[&[u8]], b: &mut [u8]) {
        b.fill(0);
        b[0..8].copy_from_slice(&JMAGIC);
        put64(b, J_SEQ, seq);
        put32(b, J_COUNT, homes.len() as u32);
        for (i, h) in homes.iter().enumerate() {
            put64(b, J_HOMES + 8 * i, *h);
        }
        let crc = Self::crc_of(b, homes.len(), images.iter().copied());
        put32(b, J_CRC, crc);
    }

    fn crc_of<'a>(b: &[u8], count: usize, images: impl Iterator<Item = &'a [u8]>) -> u32 {
        let mut c = Fnv::new()
            .update(&b[..J_CRC])
            .update(&b[J_HOMES..J_HOMES + 8 * count]);
        for i in images {
            c = c.update(i);
        }
        c.finish()
    }

    /// Reads a header block. `None` if it is not one, or lists more than
    /// `max` blocks.
    pub fn decode(b: &[u8], max: usize) -> Option<JHeader> {
        if b.len() < J_HOMES || b[0..8] != JMAGIC {
            return None;
        }
        let count = u32_at(b, J_COUNT) as usize;
        if count > max || J_HOMES + 8 * count > b.len() {
            return None;
        }
        let homes = (0..count).map(|i| u64_at(b, J_HOMES + 8 * i)).collect();
        Some(JHeader {
            seq: u64_at(b, J_SEQ),
            crc: u32_at(b, J_CRC),
            homes,
        })
    }

    /// Whether `images` (one block each, in order) are the ones the header
    /// in `b` was written with.
    pub fn verify(&self, b: &[u8], images: &[u8], bsize: usize) -> bool {
        images.len() == self.homes.len() * bsize
            && Self::crc_of(b, self.homes.len(), images.chunks(bsize)) == self.crc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn entry() -> Dentry {
        Dentry {
            name: "hello.txt".to_string(),
            uid: "inferno".to_string(),
            gid: "inferno".to_string(),
            muid: "glenda".to_string(),
            qpath: 77,
            qvers: 3,
            mode: 0o644 | DMAPPEND,
            atime: 1,
            mtime: 2,
            length: 12345,
            ptrs: core::array::from_fn(|i| i as u32 * 7),
        }
    }

    #[test]
    fn dentry_roundtrip() {
        let mut b = [0u8; DENTRY];
        let d = entry();
        d.encode(&mut b);
        assert_eq!(Dentry::decode(&b), Slot::Used(d));
        b[D_NAME] ^= 1;
        assert_eq!(Dentry::decode(&b), Slot::Bad("bad entry checksum"));
        assert_eq!(Dentry::decode(&[0u8; DENTRY]), Slot::Free);
    }

    #[test]
    fn longest_names_fit() {
        let mut d = entry();
        d.name = "n".repeat(NAME_MAX);
        d.uid = "u".repeat(USER_MAX);
        d.gid = "g".repeat(USER_MAX);
        d.muid = "m".repeat(USER_MAX);
        let mut b = [0u8; DENTRY];
        d.encode(&mut b);
        assert_eq!(Dentry::decode(&b), Slot::Used(d));
        assert!(!valid_name(&"n".repeat(NAME_MAX + 1)));
        assert!(!valid_name("a/b") && !valid_name(".") && !valid_name(""));
    }

    #[test]
    fn super_roundtrip() {
        let s = Super::layout(4096, 1000, 32).unwrap();
        assert_eq!((s.bstart, s.blen, s.dstart), (33, 1, 34));
        let mut b = vec![0u8; 4096];
        s.encode(&mut b);
        seal_super(&mut b);
        assert_eq!(Super::decode(&b).unwrap(), s);
        s.validate(1000 * 4096, 512).unwrap();
        assert!(s.validate(999 * 4096, 512).is_err());
        b[ROOT_OFF] = 1;
        assert!(Super::decode(&b).is_err());
    }

    #[test]
    fn journal_header() {
        let homes = [5u64, 9];
        let imgs = vec![1u8; 2 * 512];
        let mut b = vec![0u8; 512];
        JHeader::encode(4, &homes, &[&imgs[..512], &imgs[512..]], &mut b);
        let h = JHeader::decode(&b, 10).unwrap();
        assert_eq!((h.seq, h.homes.as_slice()), (4, &homes[..]));
        assert!(h.verify(&b, &imgs, 512));
        let mut bad = imgs.clone();
        bad[700] = 2;
        assert!(!h.verify(&b, &bad, 512));
        assert!(JHeader::decode(&b, 1).is_none());
    }
}
