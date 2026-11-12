//! FAT file systems, as dossrv(4) serves them: FAT12, FAT16 and FAT32,
//! read and write, with long (VFAT) names. A volume whose BPB gives no
//! 16-bit FAT size is FAT32; otherwise the cluster count decides between
//! FAT12 and FAT16. Times are kept as UTC.
//!
//! [`format`] makes empty file systems, for tests.

pub mod format;
pub mod name;

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};

pub const EEXIST: &str = "file exists";
pub const EPERM: &str = "permission denied";
pub const ENONEXIST: &str = "file does not exist";
pub const EFULL: &str = "file system full";
pub const EIO: &str = "i/o error";
pub const ENOTDIR: &str = "not a directory";
pub const ESYNTAX: &str = "file name syntax";
pub const EFORMAT: &str = "not a FAT file system";

pub const ATTR_RO: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME: u8 = 0x08;
pub const ATTR_DIR: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0f;

/// Where a volume lives.
pub trait Disk {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String>;
}

/// A file or device.
pub struct Image<F>(pub F);

impl<F: Read + Write + Seek> Disk for Image<F> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        self.0
            .seek(SeekFrom::Start(off))
            .and_then(|_| self.0.read_exact(buf))
            .map_err(|_| String::from(EIO))
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String> {
        self.0
            .seek(SeekFrom::Start(off))
            .and_then(|_| self.0.write_all(buf))
            .map_err(|_| String::from(EIO))
    }
}

impl Disk for Vec<u8> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        let s = usize::try_from(off).map_err(|_| String::from(EIO))?;
        let src = s
            .checked_add(buf.len())
            .and_then(|e| self.get(s..e))
            .ok_or_else(|| String::from(EIO))?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String> {
        let s = usize::try_from(off).map_err(|_| String::from(EIO))?;
        let dst = s
            .checked_add(buf.len())
            .and_then(|e| self.get_mut(s..e))
            .ok_or_else(|| String::from(EIO))?;
        dst.copy_from_slice(buf);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fat12,
    Fat16,
    Fat32,
}

/// A directory: the fixed root of FAT12 and FAT16, or a cluster chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DirRef {
    Root,
    Chain(u32),
}

/// Where an entry's short name slot is: its directory and index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Loc {
    pub dir: DirRef,
    pub slot: u32,
}

/// A directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ent {
    /// The long name, or the short one in lower case.
    pub name: String,
    pub short: [u8; 11],
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
    /// The last write, in seconds since 1970.
    pub mtime: u32,
    pub loc: Loc,
    /// The first slot of the entry, its long name's if it has one.
    pub first: u32,
}

impl Ent {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }

    pub fn readonly(&self) -> bool {
        self.attr & ATTR_RO != 0
    }

    /// The directory this entry is, when it is one.
    pub fn as_dir(&self) -> DirRef {
        DirRef::Chain(self.cluster)
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since 1970 for a DOS date and time; 0 for a date that makes
/// no sense.
pub fn from_dos(date: u16, time: u16) -> u32 {
    let (y, m, d) = (
        1980 + i64::from(date >> 9),
        i64::from((date >> 5) & 15),
        i64::from(date & 31),
    );
    let (h, mi, s) = (
        i64::from(time >> 11),
        i64::from((time >> 5) & 63),
        2 * i64::from(time & 31),
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return 0;
    }
    let t = days_from_civil(y, m, d) * 86400 + h * 3600 + mi * 60 + s;
    u32::try_from(t).unwrap_or(0)
}

/// The DOS date and time for seconds since 1970 (from 1980 to 2107).
pub fn to_dos(t: u32) -> (u16, u16) {
    let t = i64::from(t).max(days_from_civil(1980, 1, 1) * 86400);
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = (yoe + era * 400 + i64::from(m <= 2)).min(2107);
    let date = ((y - 1980) << 9) | (m << 5) | d;
    let time = ((secs / 3600) << 11) | (((secs / 60) % 60) << 5) | ((secs % 60) / 2);
    (date as u16, time as u16)
}

fn le16(b: &[u8], at: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// An open volume. Changes to the FAT are written by [`Vol::flush`].
pub struct Vol<D: Disk> {
    pub disk: D,
    pub kind: Kind,
    /// The byte offset of the volume on the disk.
    base: u64,
    bps: u64,
    spc: u64,
    nfats: u64,
    fat_start: u64,
    fat_secs: u64,
    root_start: u64,
    root_entries: u32,
    data_start: u64,
    /// The number of the last cluster, plus one.
    limit: u32,
    root_cluster: u32,
    fsinfo: Option<u64>,
    fat: Vec<u8>,
    dirty: BTreeSet<u64>,
    /// Where to look for a free cluster next.
    hint: u32,
    fsinfo_stale: bool,
}

impl<D: Disk> Vol<D> {
    /// The volume whose boot sector is at byte `base`.
    pub fn open(mut disk: D, base: u64) -> Result<Vol<D>, String> {
        let mut b = [0u8; 512];
        let bad = || String::from(EFORMAT);
        disk.read_at(base, &mut b).map_err(|_| bad())?;
        if b[510] != 0x55 || b[511] != 0xaa {
            return Err(bad());
        }
        let bps = u64::from(le16(&b, 11) as u16);
        let spc = u64::from(b[13]);
        let reserved = u64::from(le16(&b, 14));
        let nfats = u64::from(b[16]);
        let root_entries = le16(&b, 17);
        let total16 = le16(&b, 19);
        let fat16 = le16(&b, 22);
        let total = if total16 != 0 { total16 } else { le32(&b, 32) };
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || !spc.is_power_of_two()
            || reserved == 0
            || nfats == 0
        {
            return Err(bad());
        }
        let fat_secs = u64::from(if fat16 != 0 { fat16 } else { le32(&b, 36) });
        let root_secs = (u64::from(root_entries) * 32).div_ceil(bps);
        let data_sec = reserved + nfats * fat_secs + root_secs;
        if fat_secs == 0 || data_sec >= u64::from(total) {
            return Err(bad());
        }
        let clusters = (u64::from(total) - data_sec) / spc;
        let kind = if fat16 == 0 {
            Kind::Fat32
        } else if clusters < 4085 {
            Kind::Fat12
        } else {
            Kind::Fat16
        };
        let fat_bytes = fat_secs * bps;
        // The FAT must have an entry for every cluster.
        let need = match kind {
            Kind::Fat12 => (clusters + 2) * 3 / 2,
            Kind::Fat16 => (clusters + 2) * 2,
            Kind::Fat32 => (clusters + 2) * 4,
        };
        if need > fat_bytes || clusters == 0 {
            return Err(bad());
        }
        let mut fat = vec![0u8; fat_bytes as usize];
        disk.read_at(base + reserved * bps, &mut fat)?;
        let fsinfo = match kind {
            Kind::Fat32 => match le16(&b, 48) {
                0 | 0xffff => None,
                s => Some(u64::from(s)),
            },
            _ => None,
        };
        let root_cluster = if kind == Kind::Fat32 { le32(&b, 44) } else { 0 };
        let limit = (clusters + 2) as u32;
        if kind == Kind::Fat32 && !(2..limit).contains(&root_cluster) {
            return Err(bad());
        }
        Ok(Vol {
            disk,
            kind,
            base,
            bps,
            spc,
            nfats,
            fat_start: reserved,
            fat_secs,
            root_start: reserved + nfats * fat_secs,
            root_entries,
            data_start: data_sec,
            limit,
            root_cluster,
            fsinfo,
            fat,
            dirty: BTreeSet::new(),
            hint: 2,
            fsinfo_stale: false,
        })
    }

    pub fn root(&self) -> DirRef {
        match self.kind {
            Kind::Fat32 => DirRef::Chain(self.root_cluster),
            _ => DirRef::Root,
        }
    }

    pub fn cluster_bytes(&self) -> u64 {
        self.spc * self.bps
    }

    fn eoc(&self, v: u32) -> bool {
        match self.kind {
            Kind::Fat12 => v >= 0xff8,
            Kind::Fat16 => v >= 0xfff8,
            Kind::Fat32 => v >= 0x0fff_fff8,
        }
    }

    fn get(&self, c: u32) -> u32 {
        let c = c as usize;
        match self.kind {
            Kind::Fat12 => {
                let at = c + c / 2;
                let v = le16(&self.fat, at);
                if c % 2 == 1 { v >> 4 } else { v & 0xfff }
            }
            Kind::Fat16 => le16(&self.fat, c * 2),
            Kind::Fat32 => le32(&self.fat, c * 4) & 0x0fff_ffff,
        }
    }

    fn set(&mut self, c: u32, v: u32) {
        let c = c as usize;
        let (at, len) = match self.kind {
            Kind::Fat12 => {
                let at = c + c / 2;
                let old = le16(&self.fat, at);
                let new = if c % 2 == 1 {
                    (old & 0x000f) | ((v & 0xfff) << 4)
                } else {
                    (old & 0xf000) | (v & 0xfff)
                };
                self.fat[at..at + 2].copy_from_slice(&(new as u16).to_le_bytes());
                (at, 2)
            }
            Kind::Fat16 => {
                self.fat[c * 2..c * 2 + 2].copy_from_slice(&(v as u16).to_le_bytes());
                (c * 2, 2)
            }
            Kind::Fat32 => {
                let old = le32(&self.fat, c * 4) & 0xf000_0000;
                let new = old | (v & 0x0fff_ffff);
                self.fat[c * 4..c * 4 + 4].copy_from_slice(&new.to_le_bytes());
                (c * 4, 4)
            }
        };
        for b in [at, at + len - 1] {
            self.dirty.insert(b as u64 / self.bps);
        }
        self.fsinfo_stale = true;
    }

    fn end_mark(&self) -> u32 {
        match self.kind {
            Kind::Fat12 => 0xfff,
            Kind::Fat16 => 0xffff,
            Kind::Fat32 => 0x0fff_ffff,
        }
    }

    /// The clusters of the chain from `first`.
    pub fn chain(&self, first: u32) -> Result<Vec<u32>, String> {
        let mut out = Vec::new();
        let mut c = first;
        while c != 0 && !self.eoc(c) {
            if c < 2 || c >= self.limit || out.len() >= self.limit as usize {
                return Err(String::from(EIO));
            }
            out.push(c);
            c = self.get(c);
        }
        Ok(out)
    }

    fn cluster_off(&self, c: u32) -> u64 {
        self.base + (self.data_start + u64::from(c - 2) * self.spc) * self.bps
    }

    /// A free cluster, marked as the end of a chain and zeroed when
    /// `zero`.
    fn alloc(&mut self, zero: bool) -> Result<u32, String> {
        let n = self.limit - 2;
        for i in 0..n {
            let c = 2 + (self.hint - 2 + i) % n;
            if self.get(c) == 0 {
                self.set(c, self.end_mark());
                self.hint = c;
                if zero {
                    let z = vec![0u8; self.cluster_bytes() as usize];
                    let off = self.cluster_off(c);
                    self.disk.write_at(off, &z)?;
                }
                return Ok(c);
            }
        }
        Err(String::from(EFULL))
    }

    fn free_chain(&mut self, first: u32) -> Result<(), String> {
        for c in self.chain(first)? {
            self.set(c, 0);
        }
        Ok(())
    }

    /// Writes changed FAT sectors to every copy of the FAT.
    pub fn flush(&mut self) -> Result<(), String> {
        let dirty = std::mem::take(&mut self.dirty);
        for s in dirty {
            let bytes = &self.fat[(s * self.bps) as usize..((s + 1) * self.bps) as usize];
            for f in 0..self.nfats {
                let off = self.base + (self.fat_start + f * self.fat_secs + s) * self.bps;
                self.disk.write_at(off, bytes)?;
            }
        }
        if let (true, Some(s)) = (self.fsinfo_stale, self.fsinfo) {
            // The free count and hint are no longer known.
            let off = self.base + s * self.bps;
            let mut b = [0u8; 512];
            self.disk.read_at(off, &mut b)?;
            if &b[0..4] == b"RRaA" && &b[484..488] == b"rrAa" {
                b[488..496].fill(0xff);
                self.disk.write_at(off, &b)?;
            }
            self.fsinfo_stale = false;
        }
        Ok(())
    }

    /// The byte offset of slot `slot` of `dir`, or `None` past its end.
    fn slot_off(&self, dir: DirRef, slot: u32) -> Result<Option<u64>, String> {
        match dir {
            DirRef::Root => Ok((slot < self.root_entries)
                .then(|| self.base + self.root_start * self.bps + u64::from(slot) * 32)),
            DirRef::Chain(first) => {
                let per = self.cluster_bytes() / 32;
                let idx = u64::from(slot) / per;
                let chain = self.chain(first)?;
                Ok(chain
                    .get(idx as usize)
                    .map(|&c| self.cluster_off(c) + (u64::from(slot) % per) * 32))
            }
        }
    }

    fn read_slot(&mut self, dir: DirRef, slot: u32) -> Result<Option<[u8; 32]>, String> {
        let Some(off) = self.slot_off(dir, slot)? else {
            return Ok(None);
        };
        let mut e = [0u8; 32];
        self.disk.read_at(off, &mut e)?;
        Ok(Some(e))
    }

    fn write_slot(&mut self, dir: DirRef, slot: u32, e: &[u8; 32]) -> Result<(), String> {
        let off = self.slot_off(dir, slot)?.ok_or_else(|| String::from(EIO))?;
        self.disk.write_at(off, e)
    }

    /// Every slot of `dir`, up to its end mark.
    fn slots(&mut self, dir: DirRef) -> Result<Vec<[u8; 32]>, String> {
        let mut out: Vec<[u8; 32]> = Vec::new();
        match dir {
            DirRef::Root => {
                let mut all = vec![0u8; self.root_entries as usize * 32];
                self.disk
                    .read_at(self.base + self.root_start * self.bps, &mut all)?;
                out.extend_from_slice(all.as_chunks::<32>().0);
            }
            DirRef::Chain(first) => {
                let mut buf = vec![0u8; self.cluster_bytes() as usize];
                for c in self.chain(first)? {
                    let off = self.cluster_off(c);
                    self.disk.read_at(off, &mut buf)?;
                    out.extend_from_slice(buf.as_chunks::<32>().0);
                }
            }
        }
        if let Some(end) = out.iter().position(|e| e[0] == 0) {
            out.truncate(end);
        }
        Ok(out)
    }

    /// The entries of `dir`, without `.`, `..` and volume labels.
    pub fn list(&mut self, dir: DirRef) -> Result<Vec<Ent>, String> {
        let slots = self.slots(dir)?;
        let mut out = Vec::new();
        let mut lfn: Vec<u16> = Vec::new();
        let mut lfn_sum: Option<u8> = None;
        let mut lfn_first = 0u32;
        let mut expect = 0u8;
        for (i, e) in slots.iter().enumerate() {
            let i = i as u32;
            if e[0] == 0xe5 {
                lfn_sum = None;
                continue;
            }
            if e[11] == ATTR_LFN {
                let ord = e[0] & 0x1f;
                if e[0] & 0x40 != 0 {
                    lfn = name::lfn_part(e);
                    lfn_sum = Some(e[13]);
                    lfn_first = i;
                    expect = ord.wrapping_sub(1);
                } else if lfn_sum == Some(e[13]) && ord == expect && ord > 0 {
                    let mut part = name::lfn_part(e);
                    part.extend_from_slice(&lfn);
                    lfn = part;
                    expect -= 1;
                } else {
                    lfn_sum = None;
                }
                continue;
            }
            let short: [u8; 11] = e[..11].try_into().expect("11 bytes");
            let long = match lfn_sum.take() {
                Some(sum) if sum == name::checksum(&short) && expect == 0 => {
                    let end = lfn.iter().position(|&u| u == 0).unwrap_or(lfn.len());
                    Some((String::from_utf16_lossy(&lfn[..end]), lfn_first))
                }
                _ => None,
            };
            if e[11] & ATTR_VOLUME != 0 || &short[..2] == b". " || &short[..2] == b".." {
                continue;
            }
            let (name, first) = match long {
                Some((n, f)) if !n.is_empty() => (n, f),
                _ => (name::show_short(&short), i),
            };
            let hi = if self.kind == Kind::Fat32 {
                le16(e, 20) << 16
            } else {
                0
            };
            out.push(Ent {
                name,
                short,
                attr: e[11],
                cluster: hi | le16(e, 26),
                size: le32(e, 28),
                mtime: from_dos(le16(e, 24) as u16, le16(e, 22) as u16),
                loc: Loc { dir, slot: i },
                first,
            });
        }
        Ok(out)
    }

    /// The entry named `name` in `dir`, matched without regard to case.
    pub fn lookup(&mut self, dir: DirRef, name: &str) -> Result<Option<Ent>, String> {
        let want = name.to_lowercase();
        Ok(self
            .list(dir)?
            .into_iter()
            .find(|e| e.name.to_lowercase() == want || name::show_short(&e.short) == want))
    }

    /// The entry at `loc`, as it is now.
    pub fn get_ent(&mut self, loc: Loc) -> Result<Ent, String> {
        self.list(loc.dir)?
            .into_iter()
            .find(|e| e.loc == loc)
            .ok_or_else(|| String::from(ENONEXIST))
    }

    fn update<F: FnOnce(&mut [u8; 32])>(&mut self, loc: Loc, f: F) -> Result<(), String> {
        let mut e = self
            .read_slot(loc.dir, loc.slot)?
            .ok_or_else(|| String::from(EIO))?;
        f(&mut e);
        self.write_slot(loc.dir, loc.slot, &e)
    }

    /// Reads from the file `ent` at `off`.
    pub fn read(&mut self, ent: &Ent, off: u64, count: usize) -> Result<Vec<u8>, String> {
        let size = u64::from(ent.size);
        if off >= size {
            return Ok(Vec::new());
        }
        let n = (size - off).min(count as u64) as usize;
        let cb = self.cluster_bytes();
        let chain = self.chain(ent.cluster)?;
        let mut out = Vec::with_capacity(n);
        let mut pos = off;
        while out.len() < n {
            let idx = (pos / cb) as usize;
            let c = *chain.get(idx).ok_or_else(|| String::from(EIO))?;
            let within = pos % cb;
            let take = ((cb - within) as usize).min(n - out.len());
            let mut buf = vec![0u8; take];
            self.disk.read_at(self.cluster_off(c) + within, &mut buf)?;
            out.extend_from_slice(&buf);
            pos += take as u64;
        }
        Ok(out)
    }

    /// Writes `data` to the file at `loc` at `off`, growing it as needed,
    /// and sets its time to `now`.
    pub fn write(&mut self, loc: Loc, off: u64, data: &[u8], now: u32) -> Result<usize, String> {
        let mut ent = self.get_ent(loc)?;
        let end = off
            .checked_add(data.len() as u64)
            .filter(|&e| e <= u64::from(u32::MAX))
            .ok_or_else(|| String::from(EFULL))?;
        let cb = self.cluster_bytes();
        let mut chain = self.chain(ent.cluster)?;
        let need = end.div_ceil(cb) as usize;
        let had = chain.len();
        while chain.len() < need {
            let c = match self.alloc(false) {
                Ok(c) => c,
                Err(e) => {
                    // Give back what was taken for this write.
                    for &c in &chain[had..] {
                        self.set(c, 0);
                    }
                    if let Some(&last) = chain[..had].last() {
                        self.set(last, self.end_mark());
                    }
                    self.flush()?;
                    return Err(e);
                }
            };
            match chain.last() {
                Some(&last) => self.set(last, c),
                None => ent.cluster = c,
            }
            chain.push(c);
        }
        // Bytes between the old end and `off` read as zeros.
        let old = u64::from(ent.size);
        if off > old {
            let gap = vec![0u8; (off - old) as usize];
            self.put(&chain, old, &gap)?;
        }
        self.put(&chain, off, data)?;
        let size = end.max(old) as u32;
        let cluster = ent.cluster;
        let (d, t) = to_dos(now);
        let kind = self.kind;
        self.update(loc, |e| {
            set_cluster(e, cluster, kind);
            e[28..32].copy_from_slice(&size.to_le_bytes());
            e[22..24].copy_from_slice(&t.to_le_bytes());
            e[24..26].copy_from_slice(&d.to_le_bytes());
            e[11] |= ATTR_ARCHIVE;
        })?;
        self.flush()?;
        Ok(data.len())
    }

    fn put(&mut self, chain: &[u32], off: u64, data: &[u8]) -> Result<(), String> {
        let cb = self.cluster_bytes();
        let mut done = 0;
        while done < data.len() {
            let pos = off + done as u64;
            let c = chain[(pos / cb) as usize];
            let within = pos % cb;
            let take = ((cb - within) as usize).min(data.len() - done);
            self.disk
                .write_at(self.cluster_off(c) + within, &data[done..done + take])?;
            done += take;
        }
        Ok(())
    }

    /// Empties the file at `loc`.
    pub fn truncate(&mut self, loc: Loc, now: u32) -> Result<(), String> {
        let ent = self.get_ent(loc)?;
        self.free_chain(ent.cluster)?;
        let (d, t) = to_dos(now);
        let kind = self.kind;
        self.update(loc, |e| {
            set_cluster(e, 0, kind);
            e[28..32].fill(0);
            e[22..24].copy_from_slice(&t.to_le_bytes());
            e[24..26].copy_from_slice(&d.to_le_bytes());
        })?;
        self.flush()
    }

    /// `n` free slots in a row in `dir`, the directory grown if it can
    /// be; the index of the first.
    fn free_run(&mut self, dir: DirRef, n: u32) -> Result<u32, String> {
        let mut run = 0;
        let mut slot = 0u32;
        loop {
            match self.read_slot(dir, slot)? {
                Some(e) if e[0] == 0 || e[0] == 0xe5 => {
                    run += 1;
                    if run == n {
                        return Ok(slot + 1 - n);
                    }
                }
                Some(_) => run = 0,
                None => {
                    let DirRef::Chain(first) = dir else {
                        return Err(String::from(EFULL));
                    };
                    // A new, zeroed cluster: every slot in it is free.
                    let last = *self.chain(first)?.last().ok_or_else(|| String::from(EIO))?;
                    let c = self.alloc(true)?;
                    self.set(last, c);
                    self.flush()?;
                    continue;
                }
            }
            slot += 1;
        }
    }

    /// Makes `name` in `dir`: a directory when `attr` has [`ATTR_DIR`].
    pub fn create(&mut self, dir: DirRef, name: &str, attr: u8, now: u32) -> Result<Ent, String> {
        if !name::valid(name) {
            return Err(String::from(ESYNTAX));
        }
        let all = self.list(dir)?;
        let lower = name.to_lowercase();
        if all
            .iter()
            .any(|e| e.name.to_lowercase() == lower || name::show_short(&e.short) == lower)
        {
            return Err(String::from(EEXIST));
        }
        let (short, lfn) = match name::as_short(name) {
            Some(s) => (s, Vec::new()),
            None => {
                let short = (1..1_000_000)
                    .map(|n| name::basis(name, n))
                    .find(|s| !all.iter().any(|e| &e.short == s))
                    .ok_or_else(|| String::from(EFULL))?;
                (short, name::lfn_slots(name, name::checksum(&short)))
            }
        };
        let first = self.free_run(dir, lfn.len() as u32 + 1)?;
        let cluster = if attr & ATTR_DIR != 0 {
            let c = self.alloc(true)?;
            // `.` and `..`; the root is `..` cluster 0.
            let parent = match dir {
                DirRef::Chain(p) if p != self.root_cluster || self.kind != Kind::Fat32 => p,
                _ => 0,
            };
            let mut dots = vec![0u8; 64];
            for (i, (n, cl)) in [(*b".          ", c), (*b"..         ", parent)]
                .iter()
                .enumerate()
            {
                let mut e = [0u8; 32];
                e[..11].copy_from_slice(n);
                e[11] = ATTR_DIR;
                stamp(&mut e, now);
                set_cluster(&mut e, *cl, self.kind);
                dots[i * 32..i * 32 + 32].copy_from_slice(&e);
            }
            let off = self.cluster_off(c);
            self.disk.write_at(off, &dots)?;
            c
        } else {
            0
        };
        for (i, e) in lfn.iter().enumerate() {
            self.write_slot(dir, first + i as u32, e)?;
        }
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&short);
        e[11] = attr
            | if attr & ATTR_DIR == 0 {
                ATTR_ARCHIVE
            } else {
                0
            };
        stamp(&mut e, now);
        set_cluster(&mut e, cluster, self.kind);
        let slot = first + lfn.len() as u32;
        self.write_slot(dir, slot, &e)?;
        self.flush()?;
        self.get_ent(Loc { dir, slot })
    }

    /// Marks the slots of `ent` free.
    fn unlink(&mut self, ent: &Ent) -> Result<(), String> {
        for s in ent.first..=ent.loc.slot {
            self.update(
                Loc {
                    dir: ent.loc.dir,
                    slot: s,
                },
                |e| e[0] = 0xe5,
            )?;
        }
        Ok(())
    }

    /// Removes `ent`; a directory must be empty.
    pub fn remove(&mut self, ent: &Ent) -> Result<(), String> {
        if ent.is_dir() && !self.list(ent.as_dir())?.is_empty() {
            return Err(String::from(EPERM));
        }
        self.unlink(ent)?;
        self.free_chain(ent.cluster)?;
        self.flush()
    }

    /// Gives `ent` the name `new` in the same directory; returns it there.
    pub fn rename(&mut self, ent: &Ent, new: &str) -> Result<Ent, String> {
        if !name::valid(new) {
            return Err(String::from(ESYNTAX));
        }
        let dir = ent.loc.dir;
        if let Some(other) = self.lookup(dir, new)?
            && other.loc != ent.loc
        {
            return Err(String::from(EEXIST));
        }
        let mut old = Vec::new();
        for s in ent.first..=ent.loc.slot {
            old.push(self.read_slot(dir, s)?.ok_or_else(|| String::from(EIO))?);
        }
        let raw = *old.last().expect("the short name slot");
        self.unlink(ent)?;
        let made = match self.create(dir, new, raw[11] & !ATTR_DIR, 0) {
            Ok(m) => m,
            Err(e) => {
                // Put the old entry back.
                for (i, o) in old.iter().enumerate() {
                    self.write_slot(dir, ent.first + i as u32, o)?;
                }
                return Err(e);
            }
        };
        // The new slot takes everything but the name from the old one.
        self.update(made.loc, |e| e[11..].copy_from_slice(&raw[11..]))?;
        self.flush()?;
        self.get_ent(made.loc)
    }

    /// Sets the attribute bits in `mask` to those of `attr`.
    pub fn set_attr(&mut self, loc: Loc, attr: u8, mask: u8) -> Result<(), String> {
        self.update(loc, |e| e[11] = (e[11] & !mask) | (attr & mask))
    }

    pub fn set_mtime(&mut self, loc: Loc, t: u32) -> Result<(), String> {
        let (d, tm) = to_dos(t);
        self.update(loc, |e| {
            e[22..24].copy_from_slice(&tm.to_le_bytes());
            e[24..26].copy_from_slice(&d.to_le_bytes());
        })
    }

    /// The number of free clusters.
    pub fn free(&self) -> u32 {
        (2..self.limit).filter(|&c| self.get(c) == 0).count() as u32
    }
}

fn set_cluster(e: &mut [u8; 32], c: u32, kind: Kind) {
    e[26..28].copy_from_slice(&(c as u16).to_le_bytes());
    if kind == Kind::Fat32 {
        e[20..22].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
    }
}

fn stamp(e: &mut [u8; 32], now: u32) {
    let (d, t) = to_dos(now);
    for at in [14, 22] {
        e[at..at + 2].copy_from_slice(&t.to_le_bytes());
    }
    for at in [16, 18, 24] {
        e[at..at + 2].copy_from_slice(&d.to_le_bytes());
    }
}

#[cfg(test)]
mod tests;
