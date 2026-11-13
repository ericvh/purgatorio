//! The kfs disk format: Plan 9's kfs, as Inferno's kfs(4) serves it.
//!
//! The device is an array of blocks of one size (`bsize`, a multiple of
//! 512 up to 16384, 1024 by default). Every number is little-endian.
//!
//! - Block 0 holds, at byte 256, the text `kfs wren device\n` and the
//!   block size in decimal with a newline. Nothing else in it is used.
//! - Every other block ends with an 8-byte tag: two zero bytes, the tag
//!   kind (u16) and the qid path of the file it belongs to (u32), so a
//!   checker can tell what a block is. The rest (`bsize - 8` bytes) is the
//!   block's contents.
//! - Block 1 is the superblock: `fstart`, `fsize`, `tfree`, `qidgen`,
//!   `fsok`, and three unused words (u32 each), then the head of the free
//!   list: a count `nfree` and up to `FEPERBUF` block numbers. When the
//!   head is full, freeing one more block copies the head into that block
//!   (tagged `Tfree`) and starts a new head whose first entry points at
//!   it; entry 0 of the last head is 0.
//! - Block 2 is the root directory's block; its first entry is the root.
//! - A directory's contents are blocks of 88-byte entries (see
//!   [`Dentry`]). A file is its entry: six direct block numbers, one
//!   indirect and one double indirect block. A free slot has no `DALLOC`
//!   bit. Directories never shrink.
//!
//! Directory qid paths carry the top bit (`QPDIR`) on disk; it is not part
//! of the qid a client sees, nor of the tags.

mod check;
mod users;

#[cfg(test)]
mod tests;

pub use check::{CheckFlags, check, check_flags};
pub use users::{NOWORLD, Users};

/// Bytes of a name in an entry, with its terminating NUL.
pub const NAMELEN: usize = 28;
/// Direct blocks in an entry.
pub const NDBLOCK: usize = 6;
/// Bytes of an entry.
pub const DENTRYSIZE: usize = 88;
/// Bytes of a block's tag.
pub const TAGSIZE: usize = 8;
/// Bytes of the superblock before the free list head.
pub const SUPER1SIZE: usize = 32;
/// The largest block size.
pub const MAXBUFSIZE: usize = 16 * 1024;
/// The largest file: kfs's size is a signed 32-bit number.
pub const MAXFILESIZE: u64 = 0x7fff_ffff;

pub const SUPERADDR: u32 = 1;
pub const ROOTADDR: u32 = 2;

pub const QPDIR: u32 = 1 << 31;
pub const QPNONE: u32 = 0;
pub const QPROOT: u32 = 1;
pub const QPSUPER: u32 = 2;

/// Mode bits on disk.
pub const DALLOC: u16 = 0x8000;
pub const DDIR: u16 = 0x4000;
pub const DAPND: u16 = 0x2000;
pub const DLOCK: u16 = 0x1000;
pub const DREAD: u16 = 4;
pub const DWRITE: u16 = 2;
pub const DEXEC: u16 = 1;

/// Block tags.
pub const TNONE: u16 = 0;
pub const TSUPER: u16 = 1;
pub const TDIR: u16 = 2;
pub const TIND1: u16 = 3;
pub const TIND2: u16 = 4;
pub const TFILE: u16 = 5;
pub const TFREE: u16 = 6;

/// The text at byte 256 of block 0.
pub const MAGIC: &str = "kfs wren device\n";

/// User ids with fixed meanings.
pub const NONE: i16 = 0;

// Errors, as Inferno's kfs gives them.
pub const EACCESS: &str = "access permission denied";
pub const EALLOC: &str = "phase error -- directory entry not allocated";
pub const EBADSPC: &str = "attach -- bad specifier";
pub const ECOUNT: &str = "read/write -- count too big";
pub const EDIR1: &str = "walk -- in a non-directory";
pub const EDIR2: &str = "create -- in a non-directory";
pub const EDOT: &str = "create -- . and .. illegal names";
pub const EEMPTY: &str = "remove -- directory not empty";
pub const EENTRY: &str = "directory entry not found";
pub const EEXIST: &str = "create -- file exists";
pub const EFID: &str = "unknown fid";
pub const EFIDINUSE: &str = "fid already in use";
pub const EFULL: &str = "file system full";
pub const ELOCKED: &str = "open/create -- file is locked";
pub const EMODE: &str = "open/create -- unknown mode";
pub const ENAME: &str = "create/wstat -- bad character in file name";
pub const ENOTD: &str = "wstat -- attempt to change directory";
pub const ENOTG: &str = "wstat -- not in group";
pub const ENOTL: &str = "wstat -- attempt to change length";
pub const ENOTM: &str = "wstat -- unknown type/mode";
pub const ENOTU: &str = "wstat -- not owner";
pub const EOPEN: &str = "read/write -- on non open fid";
pub const EPHASE: &str = "phase error -- cannot happen";
pub const EQID: &str = "phase error -- qid does not match";
pub const EQIDMODE: &str = "wstat -- qid.qtype/dir.mode mismatch";
pub const ERONLY: &str = "file system read only";
pub const ETOOLONG: &str = "name too long";
pub const ETOOBIG: &str = "write -- file size limit";

pub fn get2(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub fn get4(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn put2(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn put4(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// Where the bytes are: an image file, a disk, or memory in tests.
pub trait Dev {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String>;
    /// The size in bytes.
    fn size(&mut self) -> Result<u64, String>;
}

impl Dev for Vec<u8> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        let o = usize::try_from(off).map_err(|_| String::from("i/o error"))?;
        let src = self
            .get(o..o + buf.len())
            .ok_or_else(|| String::from("i/o error"))?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String> {
        let o = usize::try_from(off).map_err(|_| String::from("i/o error"))?;
        let dst = self
            .get_mut(o..o + buf.len())
            .ok_or_else(|| String::from("i/o error"))?;
        dst.copy_from_slice(buf);
        Ok(())
    }

    fn size(&mut self) -> Result<u64, String> {
        Ok(self.len() as u64)
    }
}

/// A file (an image or a disk's `data`), read and written in place.
pub struct FileDev(pub std::fs::File);

impl Dev for FileDev {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        use std::io::{Read, Seek, SeekFrom};
        self.0
            .seek(SeekFrom::Start(off))
            .and_then(|_| self.0.read_exact(buf))
            .map_err(|e| e.to_string())
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String> {
        use std::io::{Seek, SeekFrom, Write};
        self.0
            .seek(SeekFrom::Start(off))
            .and_then(|_| self.0.write_all(buf))
            .map_err(|e| e.to_string())
    }

    fn size(&mut self) -> Result<u64, String> {
        use std::io::{Seek, SeekFrom};
        self.0.seek(SeekFrom::End(0)).map_err(|e| e.to_string())
    }
}

/// A directory entry, as it is on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dentry {
    pub name: String,
    pub uid: i16,
    pub gid: i16,
    pub mode: u16,
    /// The last user to change the file (Inferno's addition; Plan 9's kfs
    /// leaves it 0).
    pub muid: i16,
    /// The qid path, without `QPDIR`.
    pub path: u32,
    pub vers: u32,
    pub size: u32,
    pub dblock: [u32; NDBLOCK],
    pub iblock: u32,
    pub diblock: u32,
    pub atime: u32,
    pub mtime: u32,
}

const ONAME: usize = 0;
const OUID: usize = ONAME + NAMELEN;
const OGID: usize = OUID + 2;
const OMODE: usize = OGID + 2;
const OMUID: usize = OMODE + 2;
const OPATH: usize = OMUID + 2;
const OVERS: usize = OPATH + 4;
const OSIZE: usize = OVERS + 4;
const ODBLOCK: usize = OSIZE + 4;
const OIBLOCK: usize = ODBLOCK + NDBLOCK * 4;
const ODIBLOCK: usize = OIBLOCK + 4;
const OATIME: usize = ODIBLOCK + 4;
const OMTIME: usize = OATIME + 4;
const _: () = assert!(OMTIME + 4 == DENTRYSIZE);

impl Dentry {
    pub fn unpack(a: &[u8]) -> Dentry {
        let n = a[..NAMELEN].iter().position(|&b| b == 0).unwrap_or(NAMELEN);
        let mut d = Dentry {
            name: String::from_utf8_lossy(&a[..n]).into_owned(),
            uid: get2(a, OUID) as i16,
            gid: get2(a, OGID) as i16,
            mode: get2(a, OMODE),
            muid: get2(a, OMUID) as i16,
            path: get4(a, OPATH) & !QPDIR,
            vers: get4(a, OVERS),
            size: get4(a, OSIZE),
            iblock: get4(a, OIBLOCK),
            diblock: get4(a, ODIBLOCK),
            atime: get4(a, OATIME),
            mtime: get4(a, OMTIME),
            ..Dentry::default()
        };
        for (i, b) in d.dblock.iter_mut().enumerate() {
            *b = get4(a, ODBLOCK + 4 * i);
        }
        d
    }

    pub fn pack(&self, a: &mut [u8]) {
        a[..DENTRYSIZE].fill(0);
        let name = self.name.as_bytes();
        let n = name.len().min(NAMELEN - 1);
        a[..n].copy_from_slice(&name[..n]);
        put2(a, OUID, self.uid as u16);
        put2(a, OGID, self.gid as u16);
        put2(a, OMODE, self.mode);
        put2(a, OMUID, self.muid as u16);
        let path = if self.mode & DDIR != 0 {
            self.path | QPDIR
        } else {
            self.path
        };
        put4(a, OPATH, path);
        put4(a, OVERS, self.vers);
        put4(a, OSIZE, self.size);
        for (i, b) in self.dblock.iter().enumerate() {
            put4(a, ODBLOCK + 4 * i, *b);
        }
        put4(a, OIBLOCK, self.iblock);
        put4(a, ODIBLOCK, self.diblock);
        put4(a, OATIME, self.atime);
        put4(a, OMTIME, self.mtime);
    }

    pub fn allocated(&self) -> bool {
        self.mode & DALLOC != 0
    }

    pub fn is_dir(&self) -> bool {
        self.mode & DDIR != 0
    }
}

/// Where an entry is: its directory block and slot in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub addr: u32,
    pub slot: u32,
}

/// The root's entry.
pub const ROOT: Loc = Loc {
    addr: ROOTADDR,
    slot: 0,
};

/// The superblock's fields, and the block that holds them and the head of
/// the free list.
#[derive(Clone, Debug)]
pub struct Super {
    pub fstart: u32,
    pub fsize: u32,
    pub tfree: u32,
    pub qidgen: u32,
    pub fsok: u32,
    pub blk: Vec<u8>,
}

impl Super {
    pub fn nfree(&self) -> u32 {
        get4(&self.blk, SUPER1SIZE)
    }

    fn set_nfree(&mut self, n: u32) {
        put4(&mut self.blk, SUPER1SIZE, n);
    }

    pub fn free(&self, i: usize) -> u32 {
        get4(&self.blk, SUPER1SIZE + 4 + 4 * i)
    }

    fn set_free(&mut self, i: usize, a: u32) {
        put4(&mut self.blk, SUPER1SIZE + 4 + 4 * i, a);
    }
}

/// The tag of a block.
pub fn tag_of(blk: &[u8], bsize: usize) -> (u16, u32) {
    let t = bsize - TAGSIZE;
    (get2(blk, t + 2), get4(blk, t + 4))
}

/// Sets the tag of a block.
pub fn set_tag(blk: &mut [u8], bsize: usize, tag: u16, path: u32) {
    let t = bsize - TAGSIZE;
    put2(blk, t, 0);
    put2(blk, t + 2, tag);
    put4(blk, t + 4, path & !QPDIR);
}

/// Whether a block has `tag`, and if `path` is not `QPNONE`, belongs to it.
pub fn tag_ok(blk: &[u8], bsize: usize, tag: u16, path: u32) -> bool {
    let (t, p) = tag_of(blk, bsize);
    t == tag && (path == QPNONE || p & !QPDIR == path & !QPDIR)
}

/// A kfs on a device.
pub struct Fs<D: Dev> {
    pub dev: D,
    pub bsize: usize,
    pub ro: bool,
    /// The device's size in blocks.
    pub nblocks: u32,
    sb: Super,
}

/// Reads the block size from block 0, or `None` if it holds no kfs.
pub fn wren_bsize(dev: &mut impl Dev) -> Result<Option<usize>, String> {
    let mut b = [0u8; 256 + 32];
    if dev.size()? < b.len() as u64 {
        return Ok(None);
    }
    dev.read_at(0, &mut b)?;
    let Some(rest) = b[256..].strip_prefix(MAGIC.as_bytes()) else {
        return Ok(None);
    };
    let n = rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
    let s = String::from_utf8_lossy(&rest[..n]);
    match s.trim().parse::<usize>() {
        Ok(n) if n > 0 && n % 512 == 0 && n <= MAXBUFSIZE => Ok(Some(n)),
        _ => Err(String::from("bad block size")),
    }
}

impl<D: Dev> Fs<D> {
    pub fn bufsize(&self) -> usize {
        self.bsize - TAGSIZE
    }

    pub fn dirperbuf(&self) -> u32 {
        (self.bufsize() / DENTRYSIZE) as u32
    }

    pub fn indperbuf(&self) -> u32 {
        (self.bufsize() / 4) as u32
    }

    pub fn feperbuf(&self) -> usize {
        (self.bufsize() - SUPER1SIZE - 4) / 4
    }

    /// Makes an empty file system on `dev` (kfs's `-r`).
    pub fn ream(mut dev: D, bsize: usize, now: u32) -> Result<Fs<D>, String> {
        if bsize == 0 || !bsize.is_multiple_of(512) || bsize > MAXBUFSIZE {
            return Err(format!("invalid block size {bsize}"));
        }
        let nblocks = u32::try_from(dev.size()? / bsize as u64).unwrap_or(u32::MAX);
        if nblocks < 4 {
            return Err(String::from("file system device size"));
        }
        let mut b0 = vec![0u8; bsize];
        let m = format!("{MAGIC}{bsize}\n");
        b0[256..256 + m.len()].copy_from_slice(m.as_bytes());
        dev.write_at(0, &b0)?;
        let mut blk = vec![0u8; bsize];
        set_tag(&mut blk, bsize, TSUPER, QPSUPER);
        let sb = Super {
            fstart: 1,
            fsize: nblocks,
            tfree: 0,
            qidgen: 10,
            fsok: 0,
            blk,
        };
        let mut fs = Fs {
            dev,
            bsize,
            ro: false,
            nblocks,
            sb,
        };
        fs.sb.set_nfree(1);
        let mut sb = fs.sb.clone();
        for a in (SUPERADDR + 2..nblocks).rev() {
            fs.addfree(&mut sb, a)?;
        }
        fs.sb = sb;
        fs.put_super()?;
        let mut root = vec![0u8; bsize];
        set_tag(&mut root, bsize, TDIR, QPROOT);
        let d = Dentry {
            name: String::from("/"),
            uid: -1,
            gid: -1,
            muid: -1,
            mode: DALLOC | DDIR | 0o777,
            path: QPROOT,
            vers: 1,
            atime: now,
            mtime: now,
            ..Dentry::default()
        };
        d.pack(&mut root[..DENTRYSIZE]);
        fs.write_block(ROOTADDR, &root)?;
        Ok(fs)
    }

    /// Opens the file system on `dev`.
    pub fn open(mut dev: D, ro: bool) -> Result<Fs<D>, String> {
        let bsize = wren_bsize(&mut dev)?.ok_or_else(|| String::from("kfs magic in trouble"))?;
        let nblocks = u32::try_from(dev.size()? / bsize as u64).unwrap_or(u32::MAX);
        let mut fs = Fs {
            dev,
            bsize,
            ro,
            nblocks,
            sb: Super {
                fstart: 0,
                fsize: 0,
                tfree: 0,
                qidgen: 0,
                fsok: 0,
                blk: Vec::new(),
            },
        };
        let trouble = || String::from("kfs super/root in trouble");
        if nblocks <= ROOTADDR {
            return Err(trouble());
        }
        let blk = fs.read_block(SUPERADDR)?;
        if !tag_ok(&blk, bsize, TSUPER, QPSUPER) {
            return Err(trouble());
        }
        fs.sb = Super {
            fstart: get4(&blk, 0),
            fsize: get4(&blk, 4),
            tfree: get4(&blk, 8),
            qidgen: get4(&blk, 12),
            fsok: get4(&blk, 16),
            blk,
        };
        let root = fs.read_block(ROOTADDR)?;
        if !tag_ok(&root, bsize, TDIR, QPROOT) {
            return Err(trouble());
        }
        let d = Dentry::unpack(&root);
        if !d.allocated() || !d.is_dir() {
            return Err(String::from("bad root: not a directory"));
        }
        Ok(fs)
    }

    pub fn superblock(&self) -> &Super {
        &self.sb
    }

    /// Sets `fsok`, writing the superblock, and returns the old value.
    pub fn set_fsok(&mut self, ok: bool) -> Result<bool, String> {
        let old = self.sb.fsok != 0;
        self.sb.fsok = u32::from(ok);
        if !self.ro {
            self.put_super()?;
        }
        Ok(old)
    }

    pub fn read_block(&mut self, a: u32) -> Result<Vec<u8>, String> {
        if a >= self.nblocks {
            return Err(format!("block {a} out of range"));
        }
        let mut b = vec![0u8; self.bsize];
        self.dev.read_at(u64::from(a) * self.bsize as u64, &mut b)?;
        Ok(b)
    }

    pub fn write_block(&mut self, a: u32, b: &[u8]) -> Result<(), String> {
        if self.ro {
            return Err(String::from(ERONLY));
        }
        if a >= self.nblocks {
            return Err(format!("block {a} out of range"));
        }
        self.dev.write_at(u64::from(a) * self.bsize as u64, b)
    }

    pub fn put_super(&mut self) -> Result<(), String> {
        let mut blk = core::mem::take(&mut self.sb.blk);
        put4(&mut blk, 0, self.sb.fstart);
        put4(&mut blk, 4, self.sb.fsize);
        put4(&mut blk, 8, self.sb.tfree);
        put4(&mut blk, 12, self.sb.qidgen);
        put4(&mut blk, 16, self.sb.fsok);
        let r = self.write_block(SUPERADDR, &blk);
        self.sb.blk = blk;
        r
    }

    /// Puts `addr` on the free list head in `sb`.
    fn addfree(&mut self, sb: &mut Super, addr: u32) -> Result<(), String> {
        if addr >= sb.fsize || addr <= SUPERADDR {
            return Ok(());
        }
        let fe = self.feperbuf();
        let mut n = sb.nfree() as usize;
        if n > fe {
            return Err(String::from("addfree: bad freelist"));
        }
        if n >= fe {
            let mut b = vec![0u8; self.bsize];
            let head = SUPER1SIZE..SUPER1SIZE + (fe + 1) * 4;
            b[..(fe + 1) * 4].copy_from_slice(&sb.blk[head.clone()]);
            set_tag(&mut b, self.bsize, TFREE, QPNONE);
            self.write_block(addr, &b)?;
            sb.blk[head].fill(0);
            n = 0;
        }
        sb.set_free(n, addr);
        sb.set_nfree(n as u32 + 1);
        sb.tfree += 1;
        Ok(())
    }

    /// Takes a block off the free list, zeroes it and tags it.
    pub fn balloc(&mut self, tag: u16, path: u32) -> Result<u32, String> {
        let fe = self.feperbuf();
        let n = self.sb.nfree() as usize;
        if n == 0 || n > fe {
            return Err(String::from(EFULL));
        }
        let n = n - 1;
        let a = self.sb.free(n);
        if n == 0 {
            if a == 0 {
                self.sb.tfree = 0;
                self.put_super()?;
                return Err(String::from(EFULL));
            }
            let b = self.read_block(a)?;
            if !tag_ok(&b, self.bsize, TFREE, QPNONE) {
                return Err(String::from(EFULL));
            }
            let head = SUPER1SIZE..SUPER1SIZE + (fe + 1) * 4;
            self.sb.blk[head].copy_from_slice(&b[..(fe + 1) * 4]);
        } else {
            self.sb.set_nfree(n as u32);
        }
        self.sb.tfree = self.sb.tfree.saturating_sub(1);
        self.put_super()?;
        let mut b = vec![0u8; self.bsize];
        set_tag(&mut b, self.bsize, tag, path);
        self.write_block(a, &b)?;
        Ok(a)
    }

    /// Frees blocks, children of indirect blocks first.
    fn bfree(&mut self, list: &[(u32, u32)]) -> Result<(), String> {
        let mut all = Vec::new();
        for &(a, depth) in list {
            self.collect(a, depth, &mut all)?;
        }
        if all.is_empty() {
            return Ok(());
        }
        let mut sb = self.sb.clone();
        for a in all {
            self.addfree(&mut sb, a)?;
        }
        self.sb = sb;
        self.put_super()
    }

    fn collect(&mut self, a: u32, depth: u32, out: &mut Vec<u32>) -> Result<(), String> {
        if a == 0 || a >= self.nblocks {
            return Ok(());
        }
        if depth > 0 {
            let b = self.read_block(a)?;
            for i in (0..self.indperbuf() as usize).rev() {
                self.collect(get4(&b, 4 * i), depth - 1, out)?;
            }
        }
        out.push(a);
        Ok(())
    }

    /// A new qid path.
    pub fn qidpathgen(&mut self) -> Result<u32, String> {
        self.sb.qidgen += 1;
        self.put_super()?;
        Ok(self.sb.qidgen)
    }

    /// The entry at `loc`, if its block is a directory block and the entry
    /// is in use. With `path` not `QPNONE`, it must be that file.
    pub fn getd(&mut self, loc: Loc, path: u32) -> Result<Dentry, String> {
        if loc.slot >= self.dirperbuf() {
            return Err(String::from(EPHASE));
        }
        let b = self.read_block(loc.addr)?;
        if !tag_ok(&b, self.bsize, TDIR, QPNONE) {
            return Err(String::from(EPHASE));
        }
        let o = loc.slot as usize * DENTRYSIZE;
        let d = Dentry::unpack(&b[o..o + DENTRYSIZE]);
        if !d.allocated() {
            return Err(String::from(EALLOC));
        }
        if path != QPNONE && d.path != path & !QPDIR {
            return Err(String::from(EQID));
        }
        Ok(d)
    }

    /// Writes the entry at `loc`.
    pub fn putd(&mut self, loc: Loc, d: &Dentry) -> Result<(), String> {
        let mut b = self.read_block(loc.addr)?;
        let o = loc.slot as usize * DENTRYSIZE;
        d.pack(&mut b[o..o + DENTRYSIZE]);
        self.write_block(loc.addr, &b)
    }

    /// Clears the entry at `loc`.
    fn cleard(&mut self, loc: Loc) -> Result<(), String> {
        let mut b = self.read_block(loc.addr)?;
        let o = loc.slot as usize * DENTRYSIZE;
        b[o..o + DENTRYSIZE].fill(0);
        self.write_block(loc.addr, &b)
    }

    /// The block number of block `a` of the file `d`. With `tag` not
    /// `TNONE`, a missing block is allocated with that tag, and `d` may
    /// change; the caller writes it back.
    pub fn rel2abs(&mut self, d: &mut Dentry, a: u32, tag: u16) -> Result<u32, String> {
        let path = d.path;
        let ind = self.indperbuf();
        if (a as usize) < NDBLOCK {
            let i = a as usize;
            if d.dblock[i] == 0 && tag != TNONE {
                d.dblock[i] = self.balloc(tag, path)?;
            }
            return Ok(d.dblock[i]);
        }
        let a = a - NDBLOCK as u32;
        if a < ind {
            if d.iblock == 0 && tag != TNONE {
                d.iblock = self.balloc(TIND1, path)?;
            }
            return self.indfetch(path, d.iblock, a, TIND1, tag);
        }
        let a = a - ind;
        if u64::from(a) < u64::from(ind) * u64::from(ind) {
            if d.diblock == 0 && tag != TNONE {
                d.diblock = self.balloc(TIND2, path)?;
            }
            let t = if tag != TNONE { TIND1 } else { TNONE };
            let i1 = self.indfetch(path, d.diblock, a / ind, TIND2, t)?;
            return self.indfetch(path, i1, a % ind, TIND1, tag);
        }
        if tag != TNONE {
            return Err(String::from(ETOOBIG));
        }
        Ok(0)
    }

    fn indfetch(
        &mut self,
        path: u32,
        addr: u32,
        i: u32,
        itag: u16,
        tag: u16,
    ) -> Result<u32, String> {
        if addr == 0 {
            return Ok(0);
        }
        let mut b = self.read_block(addr)?;
        if !tag_ok(&b, self.bsize, itag, path) {
            return Err(String::from(EPHASE));
        }
        let o = 4 * i as usize;
        let mut a = get4(&b, o);
        if a == 0 && tag != TNONE {
            a = self.balloc(tag, path)?;
            put4(&mut b, o, a);
            self.write_block(addr, &b)?;
        }
        Ok(a)
    }

    /// Frees every block of `d` and makes it empty.
    pub fn trunc(&mut self, d: &mut Dentry) -> Result<(), String> {
        let mut list = vec![(d.diblock, 2), (d.iblock, 1)];
        list.extend(d.dblock.iter().rev().map(|&a| (a, 0)));
        d.diblock = 0;
        d.iblock = 0;
        d.dblock = [0; NDBLOCK];
        d.size = 0;
        self.bfree(&list)
    }

    /// The allocated entries of directory `d`.
    pub fn entries(&mut self, d: &Dentry) -> Result<Vec<(Loc, Dentry)>, String> {
        let mut out = Vec::new();
        let mut d = d.clone();
        let per = self.dirperbuf();
        for a in 0.. {
            let addr = self.rel2abs(&mut d, a, TNONE)?;
            if addr == 0 {
                break;
            }
            let b = self.read_block(addr)?;
            if !tag_ok(&b, self.bsize, TDIR, d.path) {
                return Err(String::from(EPHASE));
            }
            for slot in 0..per {
                let o = slot as usize * DENTRYSIZE;
                let e = Dentry::unpack(&b[o..o + DENTRYSIZE]);
                if e.allocated() {
                    out.push((Loc { addr, slot }, e));
                }
            }
        }
        Ok(out)
    }

    /// The entry called `name` in directory `d`.
    pub fn lookup(&mut self, d: &Dentry, name: &str) -> Result<Option<(Loc, Dentry)>, String> {
        Ok(self.entries(d)?.into_iter().find(|(_, e)| e.name == name))
    }

    /// Makes an entry in the directory at `ploc`; `e`'s name, owner, mode
    /// and times are used. Gives it a new qid path and returns where it is.
    /// The directory itself is not marked as changed here.
    pub fn create(&mut self, ploc: Loc, e: &Dentry) -> Result<(Loc, Dentry), String> {
        let mut p = self.getd(ploc, QPNONE)?;
        if !p.is_dir() {
            return Err(String::from(EDIR2));
        }
        let per = self.dirperbuf();
        let mut free = None;
        let mut a = 0;
        loop {
            let mut addr = self.rel2abs(&mut p, a, TNONE)?;
            if addr == 0 {
                if free.is_some() {
                    break;
                }
                addr = self.rel2abs(&mut p, a, TDIR)?;
                self.putd(ploc, &p)?;
            }
            let b = self.read_block(addr)?;
            if !tag_ok(&b, self.bsize, TDIR, p.path) {
                return Err(String::from(EPHASE));
            }
            for slot in 0..per {
                let o = slot as usize * DENTRYSIZE;
                let x = Dentry::unpack(&b[o..o + DENTRYSIZE]);
                if !x.allocated() {
                    if free.is_none() {
                        free = Some(Loc { addr, slot });
                    }
                } else if x.name == e.name {
                    return Err(String::from(EEXIST));
                }
            }
            a += 1;
        }
        let loc = free.expect("a free slot");
        let mut d = e.clone();
        d.mode |= DALLOC;
        d.path = self.qidpathgen()?;
        d.vers = 0;
        d.size = 0;
        d.dblock = [0; NDBLOCK];
        d.iblock = 0;
        d.diblock = 0;
        self.putd(loc, &d)?;
        Ok((loc, d))
    }

    /// Removes the entry at `loc`, freeing its blocks. A directory must be
    /// empty.
    pub fn remove(&mut self, loc: Loc) -> Result<(), String> {
        let mut d = self.getd(loc, QPNONE)?;
        if d.is_dir() && !self.entries(&d)?.is_empty() {
            return Err(String::from(EEMPTY));
        }
        self.trunc(&mut d)?;
        self.cleard(loc)
    }

    /// Reads up to `count` bytes of file `d` at `off`.
    pub fn read(&mut self, d: &Dentry, off: u64, count: usize) -> Result<Vec<u8>, String> {
        let size = u64::from(d.size);
        if off >= size {
            return Ok(Vec::new());
        }
        let count = count.min((size - off) as usize);
        let bs = self.bufsize() as u64;
        let mut d = d.clone();
        let mut out = Vec::with_capacity(count);
        let mut off = off;
        while out.len() < count {
            let a = (off / bs) as u32;
            let o = (off % bs) as usize;
            let n = (bs as usize - o).min(count - out.len());
            let addr = self.rel2abs(&mut d, a, TNONE)?;
            if addr == 0 {
                out.resize(out.len() + n, 0);
            } else {
                let b = self.read_block(addr)?;
                if !tag_ok(&b, self.bsize, TFILE, QPNONE) {
                    return Err(String::from(EPHASE));
                }
                out.extend_from_slice(&b[o..o + n]);
            }
            off += n as u64;
        }
        Ok(out)
    }

    /// Writes `data` at `off` in the file at `loc` (at its end when it is
    /// append-only), as user `uid` at time `now`. Returns the count.
    pub fn write(
        &mut self,
        loc: Loc,
        path: u32,
        off: u64,
        data: &[u8],
        uid: i16,
        now: u32,
    ) -> Result<usize, String> {
        let mut d = self.getd(loc, path)?;
        let off = if d.mode & DAPND != 0 {
            u64::from(d.size)
        } else {
            off
        };
        let end = off + data.len() as u64;
        if end > MAXFILESIZE {
            return Err(String::from(ETOOBIG));
        }
        d.atime = now;
        d.mtime = now;
        d.muid = uid;
        d.vers = d.vers.wrapping_add(1);
        if end > u64::from(d.size) {
            d.size = end as u32;
        }
        let bs = self.bufsize() as u64;
        let mut done = 0;
        let mut res = Ok(());
        while done < data.len() {
            let pos = off + done as u64;
            let a = (pos / bs) as u32;
            let o = (pos % bs) as usize;
            let n = (bs as usize - o).min(data.len() - done);
            let addr = match self.rel2abs(&mut d, a, TFILE) {
                Ok(a) => a,
                Err(e) => {
                    res = Err(e);
                    break;
                }
            };
            let mut b = self.read_block(addr)?;
            if !tag_ok(&b, self.bsize, TFILE, d.path) {
                res = Err(String::from(EALLOC));
                break;
            }
            b[o..o + n].copy_from_slice(&data[done..done + n]);
            self.write_block(addr, &b)?;
            done += n;
        }
        self.putd(loc, &d)?;
        res.map(|_| done)
    }

    /// Reads a whole file by path from the root, if it is there.
    pub fn read_path(&mut self, path: &[&str]) -> Result<Option<Vec<u8>>, String> {
        let mut d = self.getd(ROOT, QPROOT)?;
        for name in path {
            if !d.is_dir() {
                return Ok(None);
            }
            match self.lookup(&d, name)? {
                Some((_, e)) => d = e,
                None => return Ok(None),
            }
        }
        if d.is_dir() {
            return Ok(None);
        }
        self.read(&d, 0, d.size as usize).map(Some)
    }

    /// Rebuilds the free list from a map of the blocks in use (check's
    /// `f`). `used(a)` says whether block `a` is used.
    pub fn rebuild_free(&mut self, used: impl Fn(u32) -> bool) -> Result<(), String> {
        let fe = self.feperbuf();
        let mut sb = self.sb.clone();
        sb.blk[SUPER1SIZE..SUPER1SIZE + (fe + 1) * 4].fill(0);
        sb.tfree = 0;
        sb.set_nfree(1);
        for a in (sb.fstart..sb.fsize).rev() {
            if !used(a) {
                self.addfree(&mut sb, a)?;
            }
        }
        self.sb = sb;
        self.put_super()
    }

    pub fn set_qidgen(&mut self, q: u32) -> Result<(), String> {
        self.sb.qidgen = q;
        self.put_super()
    }
}
