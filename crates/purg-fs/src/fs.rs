//! The file system proper: lookups, reads and writes over a [`Disk`], with
//! a bounded cache of metadata blocks and a journal that makes each change
//! atomic.
//!
//! Every change runs as a transaction ([`Fs::op`]): the metadata blocks it
//! changes (directory blocks, indirect blocks, the free map, block 0) are
//! copied into the transaction and changed there, while the data blocks
//! it writes go straight to the disk. A commit then
//!
//! 1. flushes the disk, so the transaction's data, and the home writes of
//!    the transaction before, are stable;
//! 2. writes the journal: a header with the home block numbers and a CRC,
//!    and the images, in one write;
//! 3. flushes again, which makes the transaction durable;
//! 4. writes each image to its home block.
//!
//! At mount, a journal whose CRC matches is written home again (replaying
//! it is idempotent: it holds the newest copy of each of its blocks).
//! Blocks freed by a transaction are not reused until it commits, and no
//! block in the journal is freed and rewritten as data without a later
//! transaction replacing the journal first, so a crash at any point leaves
//! the metadata as it was before or after the last transaction. File data
//! is not journaled: an overwrite in place can be half done, but new
//! blocks are written before the metadata that points at them.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use purg_abi::error;

use crate::layout::{
    DENTRY, DMAPPEND, DMDIR, Dentry, JBLOCKS, JHeader, MODE_MASK, NDIRECT, NPTR, ROOT_OFF, Slot,
    Super, ptr_at, seal_super, set_ptr, valid_name, valid_user,
};
use crate::{Disk, Error, Result};

/// Data blocks a write adds, and blocks a truncation frees, in one
/// transaction. Bounds a transaction's metadata blocks well under the
/// journal's capacity.
pub const STEP: u64 = 16;
/// How stale an access time may get before an open updates it, in
/// seconds: reads never write the disk, and opens rarely do.
pub const ATIME_SLACK: u32 = 60;
/// The default number of metadata blocks cached.
pub const CACHE_BLOCKS: usize = 32;

/// Inferno's open modes, as `purg_abi::mode` has them.
const OREAD: u8 = 0;
const OWRITE: u8 = 1;
const ORDWR: u8 = 2;
const OTRUNC: u8 = 0x10;

/// Permission bits.
pub const R: u32 = 4;
pub const W: u32 = 2;
pub const X: u32 = 1;

fn err(s: &'static str) -> Error {
    Cow::Borrowed(s)
}

/// Where a directory entry is: its block (0 for the root, whose entry is
/// in block 0), its slot, and its qid path, which a stale reference fails
/// to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref {
    pub block: u32,
    pub slot: u16,
    pub path: u64,
}

/// A file and its parent directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub me: Ref,
    pub parent: Ref,
}

/// How [`Fs::format`] lays out a new file system.
#[derive(Clone, Debug)]
pub struct Format {
    pub bsize: usize,
    /// Journal blocks.
    pub jblocks: u32,
    /// The root's owner and group.
    pub user: String,
    /// The root's times, in seconds since the epoch.
    pub now: u32,
}

impl Default for Format {
    fn default() -> Format {
        Format {
            bsize: crate::layout::BSIZE,
            jblocks: JBLOCKS,
            user: String::from("inferno"),
            now: 0,
        }
    }
}

/// How [`Fs::mount`] opens one.
#[derive(Clone, Debug)]
pub struct Mount {
    pub read_only: bool,
    /// Metadata blocks to cache (at least 4).
    pub cache_blocks: usize,
}

impl Default for Mount {
    fn default() -> Mount {
        Mount {
            read_only: false,
            cache_blocks: CACHE_BLOCKS,
        }
    }
}

/// Space, in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub bsize: u32,
    /// Blocks in the file system, and those that can hold data.
    pub blocks: u64,
    pub data: u64,
    pub free: u64,
}

/// Changes [`Fs::wstat`] makes; `None` leaves a field alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WStat {
    pub name: Option<String>,
    pub uid: Option<String>,
    pub gid: Option<String>,
    pub mode: Option<u32>,
    pub length: Option<u64>,
    pub mtime: Option<u32>,
    pub atime: Option<u32>,
}

/// Committed metadata blocks, least recently used out first.
struct Cache {
    map: BTreeMap<u64, (u64, Vec<u8>)>,
    cap: usize,
    tick: u64,
}

impl Cache {
    fn get(&mut self, b: u64) -> Option<&[u8]> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&b).map(|e| {
            e.0 = tick;
            &e.1[..]
        })
    }

    fn insert(&mut self, b: u64, data: Vec<u8>) {
        self.map.remove(&b);
        while self.map.len() >= self.cap {
            let Some(old) = self.map.iter().min_by_key(|e| (e.1).0).map(|e| *e.0) else {
                break;
            };
            self.map.remove(&old);
        }
        self.tick += 1;
        self.map.insert(b, (self.tick, data));
    }

    fn remove(&mut self, b: u64) {
        self.map.remove(&b);
    }
}

/// A mounted file system.
pub struct Fs<D: Disk> {
    disk: D,
    pub(crate) sb: Super,
    /// `sb` as of the last commit, for an abort.
    sb_saved: Super,
    sb_dirty: bool,
    pub(crate) bs: usize,
    ro: bool,
    /// A commit failed part way: the disk's state is unknown until a
    /// remount replays the journal, so nothing more is written.
    broken: bool,
    now: u32,
    cache: Cache,
    /// On a read-only mount, the journal's images, which cannot be
    /// written home.
    pub(crate) overlay: BTreeMap<u64, Vec<u8>>,
    /// The transaction's metadata blocks.
    txn: BTreeMap<u64, Vec<u8>>,
    /// Images a transaction may hold: what the journal can.
    jcap: usize,
    /// Blocks the transaction freed, and how many it allocated.
    txn_freed: Vec<u32>,
    txn_alloc: u64,
    /// Free data blocks as of the last commit.
    free: u64,
    /// Where the next allocation looks first.
    rotor: u64,
    /// The last transaction's sequence number.
    pub(crate) seq: u64,
    /// Whether the mount replayed a journal.
    pub(crate) replayed: Option<u64>,
    scratch: Vec<u8>,
}

/// Whether `user` may `want` (bits of [`R`], [`W`], [`X`]) on `d`: the
/// owner's bits if `user` owns it, else the group's if it is `user`'s
/// group, else the others'.
pub fn perm_ok(d: &Dentry, user: &str, want: u32) -> bool {
    let bits = if d.uid == user {
        d.mode >> 6
    } else if d.gid == user {
        d.mode >> 3
    } else {
        d.mode
    };
    bits & 7 & want == want
}

impl<D: Disk> Fs<D> {
    /// Makes a new, empty file system over all of `disk`, and mounts it.
    pub fn format(mut disk: D, f: &Format) -> Result<Fs<D>> {
        if disk.read_only() {
            return Err(err(error::EPERM));
        }
        let bs = f.bsize;
        let sector = disk.sector();
        if sector == 0 || !bs.is_multiple_of(sector) || bs < sector {
            return Err(err("block size is not a multiple of the device's"));
        }
        if !valid_user(&f.user) {
            return Err(err(error::EBADARG));
        }
        let nblocks = disk.size() / bs as u64;
        let sb = Super::layout(bs, nblocks, f.jblocks)?;
        let root = Dentry {
            name: String::from("/"),
            uid: f.user.clone(),
            gid: f.user.clone(),
            muid: f.user.clone(),
            qpath: 0,
            qvers: 0,
            mode: DMDIR | 0o775,
            atime: f.now,
            mtime: f.now,
            length: 0,
            ptrs: [0; NPTR],
        };
        // The free map: the metadata blocks, and the bits past the end.
        let per = 8 * bs as u64;
        let mut map = vec![0u8; bs];
        for k in 0..u64::from(sb.blen) {
            map.fill(0);
            for bit in 0..per {
                let b = k * per + bit;
                if b < sb.dstart || b >= nblocks {
                    map[(bit / 8) as usize] |= 1 << (bit % 8);
                }
            }
            disk.write((sb.bstart + k) * bs as u64, &map)?;
        }
        // No journal yet.
        disk.write(sb.jstart * bs as u64, &vec![0u8; bs])?;
        let mut b0 = vec![0u8; bs];
        sb.encode(&mut b0);
        root.encode(&mut b0[ROOT_OFF..ROOT_OFF + DENTRY]);
        seal_super(&mut b0);
        disk.flush()?;
        disk.write(0, &b0)?;
        disk.flush()?;
        Fs::mount(disk, &Mount::default())
    }

    /// Mounts the file system on `disk`, replaying its journal.
    pub fn mount(mut disk: D, m: &Mount) -> Result<Fs<D>> {
        let sector = disk.sector();
        if sector == 0 || sector > crate::layout::MAX_BSIZE {
            return Err(err("bad device sector size"));
        }
        let mut b0 = vec![0u8; sector.max(512)];
        disk.read(0, &mut b0)?;
        let sb = Super::decode(&b0)?;
        sb.validate(disk.size(), sector)?;
        let bs = sb.bsize as usize;
        let ro = m.read_only || disk.read_only();
        let jcap = JHeader::capacity(bs, sb.jlen);
        let mut fs = Fs {
            disk,
            sb_saved: sb.clone(),
            sb,
            sb_dirty: false,
            bs,
            ro,
            broken: false,
            now: 0,
            cache: Cache {
                map: BTreeMap::new(),
                cap: m.cache_blocks.max(4),
                tick: 0,
            },
            overlay: BTreeMap::new(),
            txn: BTreeMap::new(),
            jcap,
            txn_freed: Vec::new(),
            txn_alloc: 0,
            free: 0,
            rotor: 0,
            seq: 0,
            replayed: None,
            scratch: vec![0u8; bs],
        };
        fs.replay()?;
        // Block 0 again: the journal may have held it.
        let b0 = fs.meta(0)?.to_vec();
        let sb = Super::decode(&b0)?;
        sb.validate(fs.disk.size(), sector)?;
        if sb.bsize as usize != bs || sb.jlen != fs.sb.jlen {
            return Err(err("bad superblock"));
        }
        fs.sb_saved = sb.clone();
        fs.sb = sb;
        let root = fs.root().me;
        if !fs.dentry(root)?.is_dir() {
            return Err(err("bad root directory"));
        }
        fs.free = fs.count_free()?;
        fs.rotor = fs.sb.dstart;
        Ok(fs)
    }

    /// Writes a valid journal home (or, read-only, keeps its images in
    /// memory).
    fn replay(&mut self) -> Result<()> {
        let bs = self.bs;
        let mut hdr = vec![0u8; bs];
        self.disk.read(self.sb.jstart * bs as u64, &mut hdr)?;
        let Some(h) = JHeader::decode(&hdr, self.jcap) else {
            return Ok(());
        };
        let jend = self.sb.jstart + u64::from(self.sb.jlen);
        let bad = |b: &u64| *b >= self.sb.nblocks || (self.sb.jstart..jend).contains(b);
        if h.homes.iter().any(bad) {
            return Ok(());
        }
        let mut images = vec![0u8; bs * h.homes.len()];
        if !images.is_empty() {
            self.disk
                .read((self.sb.jstart + 1) * bs as u64, &mut images)?;
        }
        if !h.verify(&hdr, &images, bs) {
            return Ok(());
        }
        for (home, img) in h.homes.iter().zip(images.chunks(bs)) {
            if self.ro {
                self.overlay.insert(*home, img.to_vec());
            } else {
                self.disk.write(home * bs as u64, img)?;
            }
        }
        if !self.ro {
            self.disk.flush()?;
        }
        self.seq = h.seq;
        self.replayed = Some(h.seq);
        Ok(())
    }

    /// Gives back the disk.
    pub fn into_disk(self) -> D {
        self.disk
    }

    pub fn disk(&self) -> &D {
        &self.disk
    }

    /// Sets the time, in seconds since the epoch, that changes are
    /// stamped with.
    pub fn set_now(&mut self, now: u32) {
        self.now = now;
    }

    pub fn read_only(&self) -> bool {
        self.ro
    }

    /// The root directory.
    pub fn root(&self) -> Loc {
        let r = Ref {
            block: 0,
            slot: 0,
            path: 0,
        };
        Loc { me: r, parent: r }
    }

    pub fn usage(&self) -> Usage {
        Usage {
            bsize: self.sb.bsize,
            blocks: self.sb.nblocks,
            data: self.sb.nblocks - self.sb.dstart,
            free: self.free_now(),
        }
    }

    fn free_now(&self) -> u64 {
        self.free.saturating_sub(self.txn_alloc)
    }

    fn count_free(&mut self) -> Result<u64> {
        let per = 8 * self.bs as u64;
        let mut used = 0u64;
        let (lo, hi) = (self.sb.dstart, self.sb.nblocks);
        for k in 0..u64::from(self.sb.blen) {
            let base = k * per;
            let map = self.meta(self.sb.bstart + k)?;
            for (i, byte) in map.iter().enumerate() {
                for bit in 0..8 {
                    let b = base + 8 * i as u64 + bit;
                    if b >= lo && b < hi && byte & (1 << bit) != 0 {
                        used += 1;
                    }
                }
            }
        }
        Ok((self.sb.nblocks - self.sb.dstart).saturating_sub(used))
    }

    // Metadata blocks.

    pub(crate) fn meta(&mut self, b: u64) -> Result<&[u8]> {
        if self.txn.contains_key(&b) {
            return Ok(&self.txn[&b]);
        }
        if self.overlay.contains_key(&b) {
            return Ok(&self.overlay[&b]);
        }
        if self.cache.get(b).is_none() {
            if b >= self.sb.nblocks {
                return Err(err(error::EIO));
            }
            let mut v = vec![0u8; self.bs];
            self.disk.read(b * self.bs as u64, &mut v)?;
            self.cache.insert(b, v);
        }
        self.cache.get(b).ok_or(err(error::EIO))
    }

    fn reserve(&self) -> Result<()> {
        if self.txn.len() >= self.jcap {
            return Err(err("transaction too large"));
        }
        Ok(())
    }

    fn meta_mut(&mut self, b: u64) -> Result<&mut [u8]> {
        if !self.txn.contains_key(&b) {
            self.reserve()?;
            let v = self.meta(b)?.to_vec();
            self.txn.insert(b, v);
        }
        self.txn
            .get_mut(&b)
            .map(|v| &mut v[..])
            .ok_or(err(error::EIO))
    }

    fn meta_new(&mut self, b: u64) -> Result<&mut [u8]> {
        self.reserve()?;
        self.cache.remove(b);
        self.txn.insert(b, vec![0u8; self.bs]);
        self.txn
            .get_mut(&b)
            .map(|v| &mut v[..])
            .ok_or(err(error::EIO))
    }

    /// Checks that `b` is a data block.
    pub(crate) fn check_data(&self, b: u32) -> Result<u64> {
        let b = u64::from(b);
        if b < self.sb.dstart || b >= self.sb.nblocks {
            return Err(err("bad block pointer"));
        }
        Ok(b)
    }

    // Transactions.

    fn writable(&self) -> Result<()> {
        if self.ro {
            Err(err(error::EPERM))
        } else if self.broken {
            Err(err(error::EIO))
        } else {
            Ok(())
        }
    }

    /// Runs `f` as one transaction: committed if it succeeds, undone if it
    /// fails.
    pub(crate) fn op<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.writable()?;
        self.sb_saved = self.sb.clone();
        match f(self) {
            Ok(v) => {
                self.commit()?;
                Ok(v)
            }
            Err(e) => {
                self.abort();
                Err(e)
            }
        }
    }

    fn abort(&mut self) {
        self.txn.clear();
        self.txn_freed.clear();
        self.txn_alloc = 0;
        self.sb = self.sb_saved.clone();
        self.sb_dirty = false;
    }

    fn commit(&mut self) -> Result<()> {
        if self.sb_dirty || self.txn.contains_key(&0) {
            let sb = self.sb.clone();
            let b0 = self.meta_mut(0)?;
            sb.encode(b0);
            seal_super(b0);
        }
        if self.txn.is_empty() {
            self.abort();
            return Ok(());
        }
        if let Err(e) = self.write_txn() {
            self.abort();
            self.broken = true;
            return Err(e);
        }
        self.seq += 1;
        self.free = (self.free + self.txn_freed.len() as u64).saturating_sub(self.txn_alloc);
        for (b, img) in core::mem::take(&mut self.txn) {
            self.cache.insert(b, img);
        }
        self.txn_freed.clear();
        self.txn_alloc = 0;
        self.sb_dirty = false;
        self.sb_saved = self.sb.clone();
        Ok(())
    }

    fn write_txn(&mut self) -> Result<()> {
        let bs = self.bs;
        self.disk.flush()?;
        let homes: Vec<u64> = self.txn.keys().copied().collect();
        let mut j = vec![0u8; bs * (1 + homes.len())];
        {
            let (hdr, body) = j.split_at_mut(bs);
            for (dst, img) in body.chunks_mut(bs).zip(self.txn.values()) {
                dst.copy_from_slice(img);
            }
            let images: Vec<&[u8]> = body.chunks(bs).collect();
            JHeader::encode(self.seq + 1, &homes, &images, hdr);
        }
        self.disk.write(self.sb.jstart * bs as u64, &j)?;
        self.disk.flush()?;
        for (b, img) in &self.txn {
            self.disk.write(b * bs as u64, img)?;
        }
        Ok(())
    }

    /// Makes everything committed so far stable at home, then empties the
    /// journal, so the next mount has nothing to replay: a clean unmount.
    pub fn sync(&mut self) -> Result<()> {
        if self.ro || self.broken {
            return Ok(());
        }
        self.disk.flush()?;
        let empty = vec![0u8; self.bs];
        self.disk.write(self.sb.jstart * self.bs as u64, &empty)?;
        self.disk.flush()
    }

    // Allocation.

    fn alloc(&mut self) -> Result<u32> {
        if self.free_now() == 0 {
            return Err(err(error::ENOSPACE));
        }
        let per = 8 * self.bs as u64;
        let (lo, hi) = (self.sb.dstart, self.sb.nblocks);
        let mut b = self.rotor.clamp(lo, hi - 1);
        let mut scanned = 0u64;
        let freed = core::mem::take(&mut self.txn_freed);
        let mut found = None;
        while scanned < (hi - lo) + per {
            let k = b / per;
            let map = match self.meta(self.sb.bstart + k) {
                Ok(m) => m,
                Err(e) => {
                    self.txn_freed = freed;
                    return Err(e);
                }
            };
            let start = (b % per) as usize;
            let mut i = start;
            while i < 8 * map.len() {
                let byte = map[i / 8];
                if byte == 0xff {
                    i = (i / 8 + 1) * 8;
                    continue;
                }
                let cand = k * per + i as u64;
                if cand >= hi {
                    break;
                }
                if byte & (1 << (i % 8)) == 0 && cand >= lo && !freed.contains(&(cand as u32)) {
                    found = Some(cand);
                    break;
                }
                i += 1;
            }
            if found.is_some() {
                break;
            }
            scanned += per - start as u64;
            b = (k + 1) * per;
            if b >= hi {
                b = lo;
            }
        }
        self.txn_freed = freed;
        let cand = found.ok_or(err(error::ENOSPACE))?;
        let per_u = per;
        let (k, bit) = (cand / per_u, (cand % per_u) as usize);
        let bb = self.sb.bstart + k;
        self.meta_mut(bb)?[bit / 8] |= 1 << (bit % 8);
        self.rotor = cand + 1;
        self.txn_alloc += 1;
        Ok(cand as u32)
    }

    fn free_block(&mut self, b: u32) -> Result<()> {
        let blk = self.check_data(b)?;
        let per = 8 * self.bs as u64;
        let (k, bit) = (blk / per, (blk % per) as usize);
        let bb = self.sb.bstart + k;
        let map = self.meta_mut(bb)?;
        if map[bit / 8] & (1 << (bit % 8)) == 0 {
            return Err(err("freeing a free block"));
        }
        map[bit / 8] &= !(1 << (bit % 8));
        self.txn_freed.push(b);
        // A metadata block freed in the transaction that changed it is
        // not journaled.
        self.txn.remove(&blk);
        self.cache.remove(blk);
        Ok(())
    }

    fn new_block(&mut self, meta: bool) -> Result<u32> {
        let b = self.alloc()?;
        if meta {
            self.meta_new(u64::from(b))?;
        }
        Ok(b)
    }

    // Block maps.

    fn per(&self) -> u64 {
        self.bs as u64 / 4
    }

    /// The most blocks a file can have.
    pub fn max_blocks(&self) -> u64 {
        let p = self.per();
        NDIRECT as u64 + p + p * p
    }

    fn ptr(&mut self, blk: u32, i: u64) -> Result<u32> {
        let b = self.check_data(blk)?;
        let v = ptr_at(self.meta(b)?, i as usize);
        if v != 0 {
            self.check_data(v)?;
        }
        Ok(v)
    }

    fn put_ptr(&mut self, blk: u32, i: u64, v: u32) -> Result<()> {
        let b = self.check_data(blk)?;
        set_ptr(self.meta_mut(b)?, i as usize, v);
        Ok(())
    }

    /// Pointer `k` of `d` (the indirect ones), given a block if it has
    /// none and `alloc`.
    fn top(&mut self, d: &mut Dentry, k: usize, alloc: bool) -> Result<u32> {
        if d.ptrs[k] != 0 {
            self.check_data(d.ptrs[k])?;
            return Ok(d.ptrs[k]);
        }
        if alloc {
            d.ptrs[k] = self.new_block(true)?;
        }
        Ok(d.ptrs[k])
    }

    fn entry(&mut self, blk: u32, i: u64, alloc: Option<bool>) -> Result<(u32, bool)> {
        let v = self.ptr(blk, i)?;
        if v != 0 {
            return Ok((v, false));
        }
        match alloc {
            None => Ok((0, false)),
            Some(meta) => {
                let b = self.new_block(meta)?;
                self.put_ptr(blk, i, b)?;
                Ok((b, true))
            }
        }
    }

    /// The block that holds block `idx` of `d`, 0 for a hole, and whether
    /// it was just allocated. With `alloc`, a hole gets a block (zeroed in
    /// the transaction if `Some(true)`, a metadata block).
    pub(crate) fn bmap(
        &mut self,
        d: &mut Dentry,
        idx: u64,
        alloc: Option<bool>,
    ) -> Result<(u32, bool)> {
        let p = self.per();
        if idx < NDIRECT as u64 {
            let i = idx as usize;
            if d.ptrs[i] != 0 {
                self.check_data(d.ptrs[i])?;
                return Ok((d.ptrs[i], false));
            }
            return match alloc {
                None => Ok((0, false)),
                Some(meta) => {
                    d.ptrs[i] = self.new_block(meta)?;
                    Ok((d.ptrs[i], true))
                }
            };
        }
        let i = idx - NDIRECT as u64;
        if i < p {
            let ind = self.top(d, NDIRECT, alloc.is_some())?;
            if ind == 0 {
                return Ok((0, false));
            }
            return self.entry(ind, i, alloc);
        }
        let i = i - p;
        if i < p * p {
            let dind = self.top(d, NDIRECT + 1, alloc.is_some())?;
            if dind == 0 {
                return Ok((0, false));
            }
            let (l1, _) = self.entry(dind, i / p, alloc.map(|_| true))?;
            if l1 == 0 {
                return Ok((0, false));
            }
            return self.entry(l1, i % p, alloc);
        }
        Err(err(error::ENOSPACE))
    }

    /// Clears the pointer to block `idx` of `d`.
    fn clear_ptr(&mut self, d: &mut Dentry, idx: u64) -> Result<()> {
        let p = self.per();
        if idx < NDIRECT as u64 {
            d.ptrs[idx as usize] = 0;
            return Ok(());
        }
        let i = idx - NDIRECT as u64;
        if i < p {
            return self.put_ptr(d.ptrs[NDIRECT], i, 0);
        }
        let i = i - p;
        let l1 = self.ptr(d.ptrs[NDIRECT + 1], i / p)?;
        self.put_ptr(l1, i % p, 0)
    }

    /// Frees the indirect blocks that cover only blocks from `n` on, now
    /// that `d` has none there. Returns how many it freed.
    fn trim(&mut self, d: &mut Dentry, n: u64) -> Result<u64> {
        let p = self.per();
        let nd = NDIRECT as u64;
        let mut freed = 0;
        if n >= nd + p {
            let i = n - nd - p;
            if i.is_multiple_of(p) && d.ptrs[NDIRECT + 1] != 0 {
                let l1 = self.ptr(d.ptrs[NDIRECT + 1], i / p)?;
                if l1 != 0 {
                    self.free_block(l1)?;
                    self.put_ptr(d.ptrs[NDIRECT + 1], i / p, 0)?;
                    freed += 1;
                }
            }
        }
        if n <= nd + p && d.ptrs[NDIRECT + 1] != 0 {
            let l1 = self.ptr(d.ptrs[NDIRECT + 1], 0)?;
            if l1 != 0 {
                self.free_block(l1)?;
                freed += 1;
            }
            self.free_block(d.ptrs[NDIRECT + 1])?;
            d.ptrs[NDIRECT + 1] = 0;
            freed += 1;
        }
        if n <= nd && d.ptrs[NDIRECT] != 0 {
            self.free_block(d.ptrs[NDIRECT])?;
            d.ptrs[NDIRECT] = 0;
            freed += 1;
        }
        Ok(freed)
    }

    // Directory entries.

    fn ent_off(&self, r: Ref) -> Result<(u64, usize)> {
        if r.block == 0 {
            return Ok((0, ROOT_OFF));
        }
        let b = self
            .check_data(r.block)
            .map_err(|_| err(error::ENONEXIST))?;
        if usize::from(r.slot) >= self.bs / DENTRY {
            return Err(err(error::ENONEXIST));
        }
        Ok((b, usize::from(r.slot) * DENTRY))
    }

    /// The entry `r` refers to; `file does not exist` once it is gone.
    pub fn dentry(&mut self, r: Ref) -> Result<Dentry> {
        let (b, off) = self.ent_off(r)?;
        let buf = self.meta(b)?;
        match Dentry::decode(&buf[off..off + DENTRY]) {
            Slot::Used(d) if d.qpath == r.path => Ok(d),
            _ => Err(err(error::ENONEXIST)),
        }
    }

    fn put_dentry(&mut self, r: Ref, d: &Dentry) -> Result<()> {
        let (b, off) = self.ent_off(r)?;
        let buf = self.meta_mut(b)?;
        d.encode(&mut buf[off..off + DENTRY]);
        if b == 0 {
            self.sb_dirty = true;
        }
        Ok(())
    }

    /// The used entries of directory `d`, with where each is.
    pub(crate) fn entries(&mut self, d: &mut Dentry) -> Result<Vec<(Ref, Dentry)>> {
        let mut out = Vec::new();
        let per = self.bs / DENTRY;
        for idx in 0..d.length / self.bs as u64 {
            let (b, _) = self.bmap(d, idx, None)?;
            if b == 0 {
                continue;
            }
            let buf = self.meta(u64::from(b))?;
            for s in 0..per {
                if let Slot::Used(e) = Dentry::decode(&buf[s * DENTRY..(s + 1) * DENTRY]) {
                    let r = Ref {
                        block: b,
                        slot: s as u16,
                        path: e.qpath,
                    };
                    out.push((r, e));
                }
            }
        }
        Ok(out)
    }

    fn lookup(&mut self, d: &mut Dentry, name: &str) -> Result<Option<Ref>> {
        Ok(self
            .entries(d)?
            .into_iter()
            .find(|(_, e)| e.name == name)
            .map(|(r, _)| r))
    }

    /// A free slot in directory `d`, which grows by a block if it has
    /// none.
    fn free_slot(&mut self, d: &mut Dentry) -> Result<(u32, u16)> {
        let per = self.bs / DENTRY;
        let nb = d.length / self.bs as u64;
        for idx in 0..nb {
            let (b, _) = self.bmap(d, idx, None)?;
            if b == 0 {
                continue;
            }
            let buf = self.meta(u64::from(b))?;
            if let Some(s) = (0..per).find(|s| {
                buf[s * DENTRY] == 0 && buf[s * DENTRY..(s + 1) * DENTRY].iter().all(|&c| c == 0)
            }) {
                return Ok((b, s as u16));
            }
        }
        let (b, _) = self.bmap(d, nb, Some(true))?;
        d.length += self.bs as u64;
        Ok((b, 0))
    }

    // Operations.

    /// Walks from directory `dir` to `name`. Needs search (execute)
    /// permission on `dir`.
    pub fn walk(&mut self, dir: Loc, name: &str, user: &str) -> Result<Loc> {
        let mut d = self.dentry(dir.me)?;
        if !d.is_dir() {
            return Err(err(error::ENOTDIR));
        }
        if !perm_ok(&d, user, X) {
            return Err(err(error::EPERM));
        }
        match self.lookup(&mut d, name)? {
            Some(r) => Ok(Loc {
                me: r,
                parent: dir.me,
            }),
            None => Err(err(error::ENONEXIST)),
        }
    }

    /// The entries of directory `dir`.
    pub fn readdir(&mut self, dir: Loc) -> Result<Vec<(Loc, Dentry)>> {
        let mut d = self.dentry(dir.me)?;
        if !d.is_dir() {
            return Err(err(error::ENOTDIR));
        }
        Ok(self
            .entries(&mut d)?
            .into_iter()
            .map(|(r, e)| {
                (
                    Loc {
                        me: r,
                        parent: dir.me,
                    },
                    e,
                )
            })
            .collect())
    }

    /// Checks that `user` may open `r` in Inferno mode `omode`, and
    /// returns its entry.
    pub fn open_check(&mut self, r: Ref, user: &str, omode: u8) -> Result<Dentry> {
        let d = self.dentry(r)?;
        let mut want = match omode & 3 {
            OREAD => R,
            OWRITE => W,
            ORDWR => R | W,
            _ => X,
        };
        if omode & OTRUNC != 0 {
            want |= W;
        }
        let writes = want & W != 0;
        if d.is_dir() && (writes || omode & 3 == 3) {
            return Err(err(error::EISDIR));
        }
        if !perm_ok(&d, user, want) {
            return Err(err(error::EPERM));
        }
        if writes {
            self.writable()?;
        }
        Ok(d)
    }

    /// Stamps `r`'s access time, if it is more than [`ATIME_SLACK`] old
    /// and the file system is writable.
    pub fn touch(&mut self, r: Ref) -> Result<()> {
        if self.writable().is_err() {
            return Ok(());
        }
        let now = self.now;
        let d = self.dentry(r)?;
        if now.saturating_sub(d.atime) < ATIME_SLACK {
            return Ok(());
        }
        self.op(|fs| {
            let mut d = fs.dentry(r)?;
            d.atime = now;
            fs.put_dentry(r, &d)
        })
    }

    /// Reads from file `r` at `off`.
    pub fn read(&mut self, r: Ref, off: u64, buf: &mut [u8]) -> Result<usize> {
        let mut d = self.dentry(r)?;
        if d.is_dir() {
            return Err(err(error::EISDIR));
        }
        if off >= d.length {
            return Ok(0);
        }
        let n = (d.length - off).min(buf.len() as u64) as usize;
        let bs = self.bs;
        let mut done = 0;
        while done < n {
            let pos = off + done as u64;
            let idx = pos / bs as u64;
            let boff = (pos % bs as u64) as usize;
            let take = (bs - boff).min(n - done);
            let (b, _) = self.bmap(&mut d, idx, None)?;
            let dst = &mut buf[done..done + take];
            if b == 0 {
                dst.fill(0);
            } else if take == bs {
                self.disk.read(u64::from(b) * bs as u64, dst)?;
            } else {
                self.disk
                    .read(u64::from(b) * bs as u64, &mut self.scratch)?;
                dst.copy_from_slice(&self.scratch[boff..boff + take]);
            }
            done += take;
        }
        Ok(n)
    }

    /// Writes `data` to file `r` at `off` (at the end, for an append-only
    /// file) as `user`. A write of more than [`STEP`] blocks is several
    /// transactions; if one fails, what went before stays and its length
    /// is returned.
    pub fn write(&mut self, r: Ref, off: u64, data: &[u8], user: &str) -> Result<usize> {
        self.writable()?;
        let d = self.dentry(r)?;
        if d.is_dir() {
            return Err(err(error::EISDIR));
        }
        let off = if d.mode & DMAPPEND != 0 {
            d.length
        } else {
            off
        };
        let bs = self.bs as u64;
        let end = off
            .checked_add(data.len() as u64)
            .ok_or(err(error::EBADARG))?;
        if end > self.max_blocks() * bs {
            return Err(err(error::ENOSPACE));
        }
        let mut done = 0usize;
        while done < data.len() {
            let pos = off + done as u64;
            let lim = (pos / bs + STEP) * bs;
            let stop = (end.min(lim) - off) as usize;
            let chunk = &data[done..stop];
            match self.op(|fs| fs.write_chunk(r, pos, chunk, user)) {
                Ok(()) => done = stop,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    }

    fn write_chunk(&mut self, r: Ref, off: u64, data: &[u8], user: &str) -> Result<()> {
        let mut d = self.dentry(r)?;
        let bs = self.bs;
        let mut done = 0;
        while done < data.len() {
            let pos = off + done as u64;
            let idx = pos / bs as u64;
            let boff = (pos % bs as u64) as usize;
            let take = (bs - boff).min(data.len() - done);
            let (b, new) = self.bmap(&mut d, idx, Some(false))?;
            let at = u64::from(b) * bs as u64;
            let src = &data[done..done + take];
            if take == bs {
                self.disk.write(at, src)?;
            } else {
                if new {
                    self.scratch.fill(0);
                } else {
                    self.disk.read(at, &mut self.scratch)?;
                }
                self.scratch[boff..boff + take].copy_from_slice(src);
                self.disk.write(at, &self.scratch)?;
            }
            done += take;
        }
        d.length = d.length.max(off + data.len() as u64);
        d.mtime = self.now;
        d.qvers = d.qvers.wrapping_add(1);
        if valid_user(user) {
            d.muid = String::from(user);
        }
        self.put_dentry(r, &d)
    }

    /// Cuts file (or empty directory) `r` to `len` bytes, [`STEP`] blocks
    /// a transaction.
    pub fn truncate(&mut self, r: Ref, len: u64) -> Result<()> {
        while !self.op(|fs| fs.shrink_step(r, len))? {}
        Ok(())
    }

    /// One transaction of [`Fs::truncate`]: frees up to [`STEP`] blocks
    /// from the end. Returns whether `r` is down to `len`.
    fn shrink_step(&mut self, r: Ref, len: u64) -> Result<bool> {
        let mut d = self.dentry(r)?;
        if len >= d.length {
            return Ok(true);
        }
        let bs = self.bs as u64;
        let p = self.per();
        let nd = NDIRECT as u64;
        let keep = len.div_ceil(bs);
        let mut n = d.length.div_ceil(bs);
        let mut freed = 0;
        while n > keep && freed < STEP {
            let idx = n - 1;
            // Skip what has no indirect block at all.
            if idx >= nd + p && d.ptrs[NDIRECT + 1] == 0 {
                n = (nd + p).max(keep);
                freed += self.trim(&mut d, n)?;
                continue;
            }
            if idx >= nd + p {
                let i = idx - nd - p;
                if self.ptr(d.ptrs[NDIRECT + 1], i / p)? == 0 {
                    n = (nd + p + (i / p) * p).max(keep);
                    freed += self.trim(&mut d, n)?;
                    continue;
                }
            } else if idx >= nd && d.ptrs[NDIRECT] == 0 {
                n = nd.max(keep);
                freed += self.trim(&mut d, n)?;
                continue;
            }
            let (b, _) = self.bmap(&mut d, idx, None)?;
            if b != 0 {
                self.free_block(b)?;
                self.clear_ptr(&mut d, idx)?;
                freed += 1;
            }
            n -= 1;
            freed += self.trim(&mut d, n)?;
        }
        let done = n <= keep;
        if done {
            d.length = len;
            // Bytes past the end of the last block read as zeros if the
            // file grows again.
            let tail = (len % bs) as usize;
            if tail != 0 && !d.is_dir() {
                let (b, _) = self.bmap(&mut d, keep - 1, None)?;
                if b != 0 {
                    let at = u64::from(b) * bs;
                    self.disk.read(at, &mut self.scratch)?;
                    self.scratch[tail..].fill(0);
                    self.disk.write(at, &self.scratch)?;
                }
            }
        } else {
            d.length = d.length.min(n * bs);
        }
        d.mtime = self.now;
        d.qvers = d.qvers.wrapping_add(1);
        self.put_dentry(r, &d)?;
        Ok(done)
    }

    /// Creates `name` in directory `dir` with Inferno permissions `perm`
    /// (`DMDIR` for a directory), owned by `user`. An existing file is
    /// truncated instead, as Inferno's create does; an existing directory
    /// is an error.
    pub fn create(&mut self, dir: Loc, name: &str, perm: u32, user: &str) -> Result<Loc> {
        self.writable()?;
        if !valid_name(name) {
            return Err(err(error::EFILENAME));
        }
        if !valid_user(user) {
            return Err(err(error::EBADARG));
        }
        let mut pd = self.dentry(dir.me)?;
        if !pd.is_dir() {
            return Err(err(error::ENOTDIR));
        }
        if !perm_ok(&pd, user, W) {
            return Err(err(error::EPERM));
        }
        if let Some(r) = self.lookup(&mut pd, name)? {
            let e = self.dentry(r)?;
            if e.is_dir() || perm & DMDIR != 0 {
                return Err(err(error::EEXIST));
            }
            if !perm_ok(&e, user, W) {
                return Err(err(error::EPERM));
            }
            self.truncate(r, 0)?;
            return Ok(Loc {
                me: r,
                parent: dir.me,
            });
        }
        let now = self.now;
        self.op(|fs| {
            let mut pd = fs.dentry(dir.me)?;
            let (block, slot) = fs.free_slot(&mut pd)?;
            let qpath = fs.sb.qidgen;
            fs.sb.qidgen += 1;
            fs.sb_dirty = true;
            let mode = if perm & DMDIR != 0 {
                perm & (!0o777 | (pd.mode & 0o777))
            } else {
                perm & (!0o666 | (pd.mode & 0o666))
            } & MODE_MASK;
            let e = Dentry {
                name: String::from(name),
                uid: String::from(user),
                gid: pd.gid.clone(),
                muid: String::from(user),
                qpath,
                qvers: 0,
                mode,
                atime: now,
                mtime: now,
                length: 0,
                ptrs: [0; NPTR],
            };
            let r = Ref {
                block,
                slot,
                path: qpath,
            };
            fs.put_dentry(r, &e)?;
            pd.qvers = pd.qvers.wrapping_add(1);
            pd.mtime = now;
            fs.put_dentry(dir.me, &pd)?;
            Ok(Loc {
                me: r,
                parent: dir.me,
            })
        })
    }

    /// Removes `loc`, which must be a file or an empty directory; `user`
    /// needs write permission on its parent. A large file is cut down
    /// [`STEP`] blocks a transaction first, so a crash part way leaves it
    /// shorter, not half removed.
    pub fn remove(&mut self, loc: Loc, user: &str) -> Result<()> {
        self.writable()?;
        if loc.me.block == 0 {
            return Err(err(error::EPERM));
        }
        let mut e = self.dentry(loc.me)?;
        let pd = self.dentry(loc.parent)?;
        if !perm_ok(&pd, user, W) {
            return Err(err(error::EPERM));
        }
        if e.is_dir() && !self.entries(&mut e)?.is_empty() {
            return Err(err(error::ENOTEMPTY));
        }
        self.truncate(loc.me, 0)?;
        let now = self.now;
        self.op(|fs| {
            fs.dentry(loc.me)?;
            let (b, off) = fs.ent_off(loc.me)?;
            fs.meta_mut(b)?[off..off + DENTRY].fill(0);
            let mut pd = fs.dentry(loc.parent)?;
            pd.qvers = pd.qvers.wrapping_add(1);
            pd.mtime = now;
            fs.put_dentry(loc.parent, &pd)
        })
    }

    /// Changes `loc`'s entry as `user`, by Inferno's rules: the name
    /// needs write permission in the parent and must be free there; the
    /// length needs write permission (and a directory's cannot change);
    /// the mode, times and group need the owner; the owner cannot change.
    /// Everything is checked before anything changes.
    pub fn wstat(&mut self, loc: Loc, w: &WStat, user: &str) -> Result<()> {
        self.writable()?;
        let d = self.dentry(loc.me)?;
        let owner = d.uid == user;
        if w.uid.as_ref().is_some_and(|u| *u != d.uid) {
            return Err(err(error::EPERM));
        }
        let rename = w.name.as_ref().filter(|n| **n != d.name);
        if let Some(n) = rename {
            if loc.me.block == 0 {
                return Err(err(error::EPERM));
            }
            if !valid_name(n) {
                return Err(err(error::EFILENAME));
            }
            let mut pd = self.dentry(loc.parent)?;
            if !perm_ok(&pd, user, W) {
                return Err(err(error::EPERM));
            }
            if self.lookup(&mut pd, n)?.is_some() {
                return Err(err(error::EEXIST));
            }
        }
        if let Some(m) = w.mode {
            if !owner || (m ^ d.mode) & DMDIR != 0 {
                return Err(err(error::EPERM));
            }
            if m & !MODE_MASK != 0 {
                return Err(err(error::EBADARG));
            }
        }
        let length = w.length.filter(|_| !d.is_dir());
        if let Some(l) = w.length {
            if d.is_dir() {
                if l != 0 {
                    return Err(err(error::EPERM));
                }
            } else if l != d.length {
                if !perm_ok(&d, user, W) {
                    return Err(err(error::EPERM));
                }
                if l > self.max_blocks() * self.bs as u64 {
                    return Err(err(error::EBADARG));
                }
            }
        }
        if (w.mtime.is_some() || w.atime.is_some()) && !owner {
            return Err(err(error::EPERM));
        }
        if let Some(g) = &w.gid {
            if !owner {
                return Err(err(error::EPERM));
            }
            if !valid_user(g) {
                return Err(err(error::EBADARG));
            }
        }
        if let Some(l) = length
            && l < d.length
        {
            self.truncate(loc.me, l)?;
        }
        let now = self.now;
        self.op(|fs| {
            let mut d = fs.dentry(loc.me)?;
            if let Some(l) = length
                && l > d.length
            {
                d.length = l;
                d.qvers = d.qvers.wrapping_add(1);
                d.mtime = now;
            }
            if let Some(m) = w.mode {
                d.mode = m;
            }
            if let Some(t) = w.mtime {
                d.mtime = t;
            }
            if let Some(t) = w.atime {
                d.atime = t;
            }
            if let Some(g) = &w.gid {
                d.gid = g.clone();
            }
            if let Some(n) = rename {
                d.name = n.clone();
                let mut pd = fs.dentry(loc.parent)?;
                pd.qvers = pd.qvers.wrapping_add(1);
                pd.mtime = now;
                fs.put_dentry(loc.parent, &pd)?;
            }
            fs.put_dentry(loc.me, &d)
        })
    }

    /// Walks `path` (`/`-separated, from the root) as `user`.
    pub fn walk_path(&mut self, path: &str, user: &str) -> Result<Loc> {
        let mut loc = self.root();
        for name in path.split('/').filter(|n| !n.is_empty()) {
            loc = self.walk(loc, name, user)?;
        }
        Ok(loc)
    }
}
