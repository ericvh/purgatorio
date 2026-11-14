//! The vac archive format (Plan 9's vac(1) and venti(6), Inferno's vac(2)
//! module): a tree of files kept in Venti.
//!
//! A file's contents are a stream of data blocks ([`Entry`]). A directory
//! is two streams: a directory stream of the entries of its children, and
//! a metadata stream of [`Direntry`] records, packed into meta blocks and
//! sorted by name. Each record names the index of its child's entry, and
//! for a directory the index of the child's metadata entry as well.
//!
//! The archive's root block (`vac:` score, [`Root`]) points at a
//! directory block of three entries: the root directory's two streams and
//! a metadata stream holding the root directory's own record.

use std::cmp::Ordering;

use purgatorio::{DMAPPEND, DMDIR, DMEXCL, Dir, Qid};

use crate::{
    DATATYPE, DIRTYPE, ENTRYACTIVE, ENTRYDIR, ENTRYSIZE, Entry, POINTERDEPTH, POINTERTYPE0,
    ROOTSIZE, ROOTTYPE, Root, SCORESIZE, Score, Venti, zero_truncate,
};

// Mode bits of a Direntry.
pub const MODEPERM: u32 = 0o777;
pub const MODESTICKY: u32 = 1 << 9;
pub const MODESETUID: u32 = 1 << 10;
pub const MODESETGID: u32 = 1 << 11;
pub const MODEAPPEND: u32 = 1 << 12;
pub const MODEEXCL: u32 = 1 << 13;
pub const MODESYMLINK: u32 = 1 << 14;
pub const MODEDIR: u32 = 1 << 15;
pub const MODEHIDDEN: u32 = 1 << 16;
pub const MODESYSTEM: u32 = 1 << 17;
pub const MODEARCHIVE: u32 = 1 << 18;
pub const MODETEMP: u32 = 1 << 19;
pub const MODESNAPSHOT: u32 = 1 << 20;
pub const MODEDEV: u32 = 1 << 21;
pub const MODENAMEDPIPE: u32 = 1 << 22;

/// Inferno's `DMTMP`.
pub const DMTMP: u32 = 0x0400_0000;

pub const DIRMAGIC: u32 = 0x1c4d_9072;
pub const METAMAGIC: u32 = 0x5656_fc79;
pub const METAHEADERSIZE: usize = 12;
pub const METAINDEXSIZE: usize = 4;
/// The optional Direntry field giving the qids a subtree uses.
const DIRQIDSPACE: u8 = 3;

/// The default block size.
pub const DSIZE: u32 = 8192;

/// The bytes of a pointer block for blocks of `bsize`: whole scores.
pub fn psize(bsize: u32) -> u32 {
    let p = bsize / SCORESIZE as u32 * SCORESIZE as u32;
    p.min(60000 / SCORESIZE as u32 * SCORESIZE as u32)
}

fn get16(b: &[u8], o: usize) -> usize {
    usize::from(u16::from_be_bytes([b[o], b[o + 1]]))
}

fn get32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// A file's metadata, Plan 9's `VacDir`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Direntry {
    pub elem: String,
    /// The index of the file's entry in its directory's stream.
    pub entry: u32,
    pub generation: u32,
    /// For a directory, the index of its metadata entry.
    pub mentry: u32,
    pub mgen: u32,
    pub qid: u64,
    pub uid: String,
    pub gid: String,
    pub mid: String,
    pub mtime: u32,
    pub mcount: u32,
    pub ctime: u32,
    pub atime: u32,
    /// The `MODE*` bits.
    pub mode: u32,
    /// `(offset, max)` of the qids the subtree uses.
    pub qidspace: Option<(u64, u64)>,
}

/// Reads the fields of a packed Direntry in order.
struct Fields<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> Fields<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.o + n > self.b.len() {
            return Err(String::from("corrupted meta block entry"));
        }
        let s = &self.b[self.o..self.o + n];
        self.o += n;
        Ok(s)
    }

    fn u16(&mut self) -> Result<usize, String> {
        Ok(get16(self.take(2)?, 0))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(get32(self.take(4)?, 0))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok((u64::from(get32(b, 0)) << 32) | u64::from(get32(b, 4)))
    }

    fn string(&mut self) -> Result<String, String> {
        let n = self.u16()?;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

fn put_string(d: &mut Vec<u8>, s: &str) {
    d.extend_from_slice(&(s.len() as u16).to_be_bytes());
    d.extend_from_slice(s.as_bytes());
}

impl Direntry {
    /// The record for a file described by `d`, as Inferno's `Direntry.mk`
    /// makes it: the permissions and the append, exclusive, directory and
    /// temporary bits; no access or change time.
    pub fn from_dir(d: &Dir) -> Direntry {
        let mut mode = d.mode & MODEPERM;
        for (dm, m) in [
            (DMAPPEND, MODEAPPEND),
            (DMEXCL, MODEEXCL),
            (DMDIR, MODEDIR),
            (DMTMP, MODETEMP),
        ] {
            if d.mode & dm != 0 {
                mode |= m;
            }
        }
        Direntry {
            elem: d.name.clone(),
            qid: d.qid.path,
            uid: d.uid.clone(),
            gid: d.gid.clone(),
            mid: d.muid.clone(),
            mtime: d.mtime,
            mode,
            ..Direntry::default()
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & MODEDIR != 0
    }

    /// The mode as a Styx file mode.
    pub fn emode(&self) -> u32 {
        let mut m = self.mode & MODEPERM;
        for (vm, dm) in [
            (MODEAPPEND, DMAPPEND),
            (MODEEXCL, DMEXCL),
            (MODEDIR, DMDIR),
            (MODETEMP, DMTMP),
        ] {
            if self.mode & vm != 0 {
                m |= dm;
            }
        }
        m
    }

    /// The Styx directory entry, with length 0.
    pub fn to_dir(&self) -> Dir {
        let emode = self.emode();
        Dir {
            name: self.elem.clone(),
            uid: self.uid.clone(),
            gid: self.gid.clone(),
            muid: self.mid.clone(),
            qid: Qid {
                path: self.qid,
                vers: 0,
                qtype: (emode >> 24) as u8,
            },
            mode: emode,
            atime: self.atime,
            mtime: self.mtime,
            ..Dir::default()
        }
    }

    /// The record in version 9 of the format: `magic[4] version[2]
    /// elem[s] entry[4] gen[4] mentry[4] mgen[4] qid[8] uid[s] gid[s]
    /// mid[s] mtime[4] mcount[4] ctime[4] atime[4] mode[4]`, then the
    /// optional qid space (`3[1] 16[2] offset[8] max[8]`). A string is
    /// `n[2]` and `n` bytes.
    pub fn pack(&self) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&DIRMAGIC.to_be_bytes());
        d.extend_from_slice(&9u16.to_be_bytes());
        put_string(&mut d, &self.elem);
        for v in [self.entry, self.generation, self.mentry, self.mgen] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&self.qid.to_be_bytes());
        for s in [&self.uid, &self.gid, &self.mid] {
            put_string(&mut d, s);
        }
        for v in [self.mtime, self.mcount, self.ctime, self.atime, self.mode] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        if let Some((off, max)) = self.qidspace {
            d.push(DIRQIDSPACE);
            d.extend_from_slice(&16u16.to_be_bytes());
            d.extend_from_slice(&off.to_be_bytes());
            d.extend_from_slice(&max.to_be_bytes());
        }
        d
    }

    /// Decodes a record of version 7, 8 or 9.
    pub fn unpack(b: &[u8]) -> Result<Direntry, String> {
        let mut f = Fields { b, o: 0 };
        let magic = f.u32()?;
        if magic != DIRMAGIC {
            return Err(format!("bad magic ({magic:x}, want {DIRMAGIC:x})"));
        }
        let version = f.u16()?;
        if !(7..=9).contains(&version) {
            return Err(format!("bad version ({version})"));
        }
        let mut de = Direntry {
            elem: f.string()?,
            entry: f.u32()?,
            ..Direntry::default()
        };
        if version < 9 {
            de.mentry = de.entry + 1;
        } else {
            de.generation = f.u32()?;
            de.mentry = f.u32()?;
            de.mgen = f.u32()?;
        }
        de.qid = f.u64()?;
        if version == 7 {
            // A replacement score, no longer used.
            f.take(SCORESIZE)?;
        }
        de.uid = f.string()?;
        de.gid = f.string()?;
        de.mid = f.string()?;
        de.mtime = f.u32()?;
        de.mcount = f.u32()?;
        de.ctime = f.u32()?;
        de.atime = f.u32()?;
        de.mode = f.u32()?;
        while f.o < b.len() {
            let t = f.take(1)?[0];
            let n = f.u16()?;
            let v = f.take(n)?;
            if t == DIRQIDSPACE {
                if n != 16 {
                    return Err(format!("invalid qidspace length {n}"));
                }
                let mut g = Fields { b: v, o: 0 };
                de.qidspace = Some((g.u64()?, g.u64()?));
            }
            // Other optional fields (Plan 9 and NT extensions of older
            // versions) are skipped.
        }
        Ok(de)
    }
}

/// Compares names as vac orders a meta block. Plan 9's vac sorts a name
/// after the names it is a prefix of (`ab` before `a`); fossil, whose
/// blocks have magic `METAMAGIC+1`, sorts it before.
pub fn elem_cmp(a: &[u8], b: &[u8], fossil: bool) -> Ordering {
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        Ordering::Equal if fossil => a.len().cmp(&b.len()),
        Ordering::Equal => b.len().cmp(&a.len()),
        o => o,
    }
}

/// A meta block: `magic[4] size[2] free[2] maxindex[2] nindex[2]`, then
/// `nindex` index entries `offset[2] size[2]` in name order, then the
/// packed Direntries they point at.
pub struct MetaBlock<'a> {
    buf: &'a [u8],
    pub size: usize,
    pub maxindex: usize,
    pub nindex: usize,
    /// Fossil's order (magic `METAMAGIC+1`).
    pub fossil: bool,
}

impl<'a> MetaBlock<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<MetaBlock<'a>, String> {
        if buf.len() < METAHEADERSIZE {
            return Err(format!(
                "bad length for metablock ({}, want {METAHEADERSIZE})",
                buf.len()
            ));
        }
        let magic = get32(buf, 0);
        if magic != METAMAGIC && magic != METAMAGIC + 1 {
            return Err(format!("bad meta block magic {magic:#010x}"));
        }
        let mb = MetaBlock {
            buf,
            size: get16(buf, 4),
            maxindex: get16(buf, 8),
            nindex: get16(buf, 10),
            fossil: magic == METAMAGIC + 1,
        };
        if mb.size > buf.len() || mb.nindex > mb.maxindex {
            return Err(String::from("bad meta block size"));
        }
        if METAHEADERSIZE + mb.maxindex * METAINDEXSIZE > buf.len() {
            return Err(String::from("truncated meta block"));
        }
        Ok(mb)
    }

    /// The packed Direntry at index `i`.
    fn raw(&self, i: usize) -> Result<&'a [u8], String> {
        if i >= self.nindex {
            return Err(String::from("bad meta entry index"));
        }
        let p = METAHEADERSIZE + i * METAINDEXSIZE;
        let (o, n) = (get16(self.buf, p), get16(self.buf, p + 2));
        if o < METAHEADERSIZE + self.maxindex * METAINDEXSIZE || o + n > self.size {
            return Err(String::from("corrupted entry in meta block"));
        }
        let e = &self.buf[o..o + n];
        if n < 8 || get32(e, 0) != DIRMAGIC || n < 8 + get16(e, 6) {
            return Err(String::from("corrupted meta block entry"));
        }
        Ok(e)
    }

    pub fn direntry(&self, i: usize) -> Result<Direntry, String> {
        Direntry::unpack(self.raw(i)?)
    }

    /// The record named `elem`, by binary search.
    pub fn search(&self, elem: &str) -> Result<Option<Direntry>, String> {
        let (mut b, mut t) = (0, self.nindex);
        while b < t {
            let i = (b + t) / 2;
            let e = self.raw(i)?;
            let name = &e[8..8 + get16(e, 6)];
            match elem_cmp(name, elem.as_bytes(), self.fossil) {
                Ordering::Equal => return self.direntry(i).map(Some),
                Ordering::Less => b = i + 1,
                Ordering::Greater => t = i,
            }
        }
        Ok(None)
    }
}

fn bad_elem(e: &str) -> bool {
    e.is_empty() || e == "." || e == ".." || e.contains('/') || e.contains('\0')
}

/// Writes a stream of blocks and the tree of pointer blocks over them.
pub struct StreamWriter {
    btype: u8,
    psize: u32,
    dsize: u32,
    /// The scores waiting at each level of the tree.
    levels: Vec<Vec<u8>>,
    size: u64,
}

impl StreamWriter {
    /// A stream of `btype` (`DATATYPE` or `DIRTYPE`) blocks of `dsize`
    /// bytes, with pointer blocks of `psize`.
    pub fn new(btype: u8, psize: u32, dsize: u32) -> StreamWriter {
        StreamWriter {
            btype,
            psize,
            dsize,
            levels: vec![Vec::new()],
            size: 0,
        }
    }

    /// Appends one block. All but the last block of a stream are `dsize`
    /// bytes long.
    pub fn write(&mut self, v: &mut dyn Venti, data: &[u8]) -> Result<(), String> {
        let score = v.write(self.btype, zero_truncate(self.btype, data))?;
        self.size += data.len() as u64;
        self.push(v, 0, score)
    }

    fn per_pointer(&self) -> usize {
        (self.psize as usize / SCORESIZE).max(2)
    }

    fn flush_level(&mut self, v: &mut dyn Venti, level: usize) -> Result<Score, String> {
        let t = POINTERTYPE0 + level as u8;
        let s = v.write(t, zero_truncate(t, &self.levels[level]))?;
        self.levels[level].clear();
        Ok(s)
    }

    fn push(&mut self, v: &mut dyn Venti, level: usize, score: Score) -> Result<(), String> {
        if level > usize::from(POINTERDEPTH) {
            return Err(String::from("file too big"));
        }
        if self.levels.len() <= level {
            self.levels.push(Vec::new());
        }
        self.levels[level].extend_from_slice(&score.0);
        if self.levels[level].len() == self.per_pointer() * SCORESIZE {
            let s = self.flush_level(v, level)?;
            self.push(v, level + 1, s)?;
        }
        Ok(())
    }

    /// Writes what is left of the tree and returns the stream's entry.
    pub fn finish(mut self, v: &mut dyn Venti) -> Result<Entry, String> {
        let mut level = 0;
        let (depth, score) = loop {
            let above = self.levels[level + 1..].iter().any(|l| !l.is_empty());
            let here = &self.levels[level];
            if !above {
                if here.is_empty() {
                    break (0, Score::zero());
                }
                if here.len() == SCORESIZE {
                    break (level, Score::from_slice(here));
                }
            }
            if !here.is_empty() {
                let s = self.flush_level(v, level)?;
                self.push(v, level + 1, s)?;
            }
            level += 1;
        };
        Ok(Entry {
            generation: 0,
            psize: self.psize,
            dsize: self.dsize,
            depth: depth as u8,
            flags: ENTRYACTIVE | if self.btype == DIRTYPE { ENTRYDIR } else { 0 },
            size: self.size,
            score,
        })
    }
}

/// Collects the entries of a directory stream.
pub struct DirSink {
    bsize: u32,
    entries: Vec<[u8; ENTRYSIZE]>,
}

impl DirSink {
    pub fn new(bsize: u32) -> DirSink {
        DirSink {
            bsize,
            entries: Vec::new(),
        }
    }

    /// Adds `e` and returns its index.
    pub fn add(&mut self, e: &Entry) -> u32 {
        self.entries.push(e.pack());
        (self.entries.len() - 1) as u32
    }

    pub fn finish(self, v: &mut dyn Venti) -> Result<Entry, String> {
        let epb = (self.bsize as usize / ENTRYSIZE).max(1);
        let mut w = StreamWriter::new(DIRTYPE, psize(self.bsize), (epb * ENTRYSIZE) as u32);
        for chunk in self.entries.chunks(epb) {
            w.write(v, chunk.as_flattened())?;
        }
        w.finish(v)
    }
}

/// Collects the Direntries of a directory's metadata stream.
pub struct MetaSink {
    bsize: u32,
    entries: Vec<Direntry>,
}

impl MetaSink {
    pub fn new(bsize: u32) -> MetaSink {
        MetaSink {
            bsize,
            entries: Vec::new(),
        }
    }

    pub fn add(&mut self, de: Direntry) {
        self.entries.push(de);
    }

    /// Sorts the records into vac's order and packs them into meta
    /// blocks of at most `bsize` bytes each.
    pub fn finish(mut self, v: &mut dyn Venti) -> Result<Entry, String> {
        self.entries
            .sort_by(|a, b| elem_cmp(a.elem.as_bytes(), b.elem.as_bytes(), false));
        let bsize = self.bsize as usize;
        let mut w = StreamWriter::new(DATATYPE, psize(self.bsize), self.bsize);
        let packed: Vec<Vec<u8>> = self.entries.iter().map(Direntry::pack).collect();
        let mut blocks: Vec<&[Vec<u8>]> = Vec::new();
        let (mut start, mut used) = (0, METAHEADERSIZE);
        for (i, p) in packed.iter().enumerate() {
            let need = METAINDEXSIZE + p.len();
            if METAHEADERSIZE + need > bsize || p.len() > 0xffff {
                return Err(format!(
                    "directory entry for {} too big",
                    self.entries[i].elem
                ));
            }
            if used + need > bsize {
                blocks.push(&packed[start..i]);
                start = i;
                used = METAHEADERSIZE;
            }
            used += need;
        }
        if start < packed.len() {
            blocks.push(&packed[start..]);
        }
        let n = blocks.len();
        for (bi, recs) in blocks.into_iter().enumerate() {
            let mut d = vec![0u8; METAHEADERSIZE + recs.len() * METAINDEXSIZE];
            for (i, r) in recs.iter().enumerate() {
                let p = METAHEADERSIZE + i * METAINDEXSIZE;
                let off = d.len();
                d[p..p + 2].copy_from_slice(&(off as u16).to_be_bytes());
                d[p + 2..p + 4].copy_from_slice(&(r.len() as u16).to_be_bytes());
                d.extend_from_slice(r);
            }
            let size = d.len();
            d[0..4].copy_from_slice(&METAMAGIC.to_be_bytes());
            d[4..6].copy_from_slice(&(size as u16).to_be_bytes());
            // free[2] is 0: the records fill the block from the index.
            d[8..10].copy_from_slice(&(recs.len() as u16).to_be_bytes());
            d[10..12].copy_from_slice(&(recs.len() as u16).to_be_bytes());
            if bi + 1 < n {
                // All but the last block take the whole block size.
                d.resize(bsize, 0);
            }
            w.write(v, &d)?;
        }
        w.finish(v)
    }
}

/// Writes the top of an archive: the metadata stream holding `top`, the
/// root directory's own record, the block of the three top-level entries
/// (`dir` and `meta` being the root directory's streams), and the root
/// block. Returns the root block's score, the archive's `vac:` score.
pub fn write_root(
    v: &mut dyn Venti,
    name: &str,
    bsize: u32,
    dir: &Entry,
    meta: &Entry,
    top: Direntry,
) -> Result<Score, String> {
    let mut ms = MetaSink::new(bsize);
    ms.add(top);
    let e2 = ms.finish(v)?;
    let mut td = Vec::with_capacity(3 * ENTRYSIZE);
    for e in [dir, meta, &e2] {
        td.extend_from_slice(&e.pack());
    }
    let tscore = v.write(DIRTYPE, zero_truncate(DIRTYPE, &td))?;
    let root = Root {
        name: String::from(name),
        rtype: String::from("vac"),
        score: tscore,
        blocksize: bsize,
        prev: None,
    };
    v.write(ROOTTYPE, &root.pack()?)
}

/// Reads a stream described by an entry.
#[derive(Clone)]
pub struct Source {
    pub e: Entry,
    /// The last block read at each level, pointer blocks above.
    cache: Vec<Option<(Score, Vec<u8>)>>,
}

impl Source {
    pub fn new(e: Entry) -> Source {
        Source {
            e,
            cache: vec![None; usize::from(POINTERDEPTH) + 2],
        }
    }

    /// Entries in each block of a directory stream.
    fn epb(&self) -> u64 {
        (u64::from(self.e.dsize) / ENTRYSIZE as u64).max(1)
    }

    /// The number of entries in a directory stream.
    pub fn nentries(&self) -> u64 {
        let ds = u64::from(self.e.dsize);
        self.epb() * (self.e.size / ds) + (self.e.size % ds) / ENTRYSIZE as u64
    }

    fn fetch(
        &mut self,
        v: &mut dyn Venti,
        slot: usize,
        score: Score,
        t: u8,
        max: usize,
    ) -> Result<Vec<u8>, String> {
        if let Some((s, d)) = &self.cache[slot]
            && *s == score
        {
            return Ok(d.clone());
        }
        let d = v.read(&score, t, max)?;
        self.cache[slot] = Some((score, d.clone()));
        Ok(d)
    }

    /// Block `i` of the stream as stored (without trailing zeros).
    fn block(&mut self, v: &mut dyn Venti, i: u64) -> Result<Vec<u8>, String> {
        let npp = u64::from(self.e.psize / SCORESIZE as u32).max(1);
        let depth = usize::from(self.e.depth);
        if depth > usize::from(POINTERDEPTH) {
            return Err(String::from("bad depth"));
        }
        // Blocks under each score of the block at this level.
        let mut per = 1u64;
        for _ in 1..depth {
            per = per.saturating_mul(npp);
        }
        let mut score = self.e.score;
        let mut i = i;
        for level in (1..=depth).rev() {
            let buf = self.fetch(
                v,
                level,
                score,
                POINTERTYPE0 + (level - 1) as u8,
                self.e.psize as usize,
            )?;
            let idx = i / per;
            if idx >= npp {
                return Ok(Vec::new());
            }
            i %= per;
            per = (per / npp).max(1);
            let o = idx as usize * SCORESIZE;
            score = if o + SCORESIZE <= buf.len() {
                Score::from_slice(&buf[o..])
            } else {
                Score::zero()
            };
        }
        let t = if self.e.is_dir() { DIRTYPE } else { DATATYPE };
        self.fetch(v, 0, score, t, self.e.dsize as usize)
    }

    /// Up to `n` bytes of a file stream from `offset`; empty at the end.
    pub fn read_at(&mut self, v: &mut dyn Venti, offset: u64, n: usize) -> Result<Vec<u8>, String> {
        let ds = u64::from(self.e.dsize);
        let end = self.e.size.min(offset.saturating_add(n as u64));
        let mut out = Vec::new();
        let mut off = offset;
        while off < end {
            let bi = off / ds;
            let mut b = self.block(v, bi)?;
            let blen = (self.e.size - bi * ds).min(ds) as usize;
            b.resize(blen, 0);
            let from = (off - bi * ds) as usize;
            let to = blen.min(from + (end - off) as usize);
            out.extend_from_slice(&b[from..to]);
            off += (to - from) as u64;
        }
        Ok(out)
    }

    /// Entry `n` of a directory stream; it must be active.
    pub fn entry(&mut self, v: &mut dyn Venti, n: u64) -> Result<Entry, String> {
        if n >= self.nentries() {
            return Err(format!("bad archive, entry={n} not present"));
        }
        let epb = self.epb();
        let b = self.block(v, n / epb)?;
        let o = (n % epb) as usize * ENTRYSIZE;
        let mut buf = [0u8; ENTRYSIZE];
        if o < b.len() {
            let m = (b.len() - o).min(ENTRYSIZE);
            buf[..m].copy_from_slice(&b[o..o + m]);
        }
        let e = Entry::unpack(&buf)?;
        if !e.is_active() {
            return Err(String::from("entry not active"));
        }
        Ok(e)
    }

    /// The number of blocks in the stream.
    pub fn nblocks(&self) -> u64 {
        self.e.size.div_ceil(u64::from(self.e.dsize))
    }
}

/// A directory of an archive: its directory and metadata streams.
#[derive(Clone)]
pub struct VacDir {
    pub dir: Source,
    pub meta: Source,
}

impl VacDir {
    pub fn new(dir: Entry, meta: Entry) -> VacDir {
        VacDir {
            dir: Source::new(dir),
            meta: Source::new(meta),
        }
    }

    /// Meta block `i`.
    fn meta_block(&mut self, v: &mut dyn Venti, i: u64) -> Result<Vec<u8>, String> {
        let ds = self.meta.e.dsize as usize;
        let mut b = self.meta.block(v, i)?;
        b.resize(ds, 0);
        Ok(b)
    }

    /// The records of every file in the directory, in stored order.
    pub fn list(&mut self, v: &mut dyn Venti) -> Result<Vec<Direntry>, String> {
        let mut out = Vec::new();
        for i in 0..self.meta.nblocks() {
            let b = self.meta_block(v, i)?;
            let mb = MetaBlock::parse(&b)?;
            for j in 0..mb.nindex {
                let de = mb.direntry(j)?;
                if bad_elem(&de.elem) {
                    return Err(format!("bad direntry: {}", de.elem));
                }
                out.push(de);
            }
        }
        Ok(out)
    }

    /// The record of `elem`.
    pub fn walk(&mut self, v: &mut dyn Venti, elem: &str) -> Result<Option<Direntry>, String> {
        for i in 0..self.meta.nblocks() {
            let b = self.meta_block(v, i)?;
            if let Some(de) = MetaBlock::parse(&b)?.search(elem)? {
                return Ok(Some(de));
            }
        }
        Ok(None)
    }

    /// The entries of the file `de` names: its stream, and for a
    /// directory its metadata stream.
    pub fn open(
        &mut self,
        v: &mut dyn Venti,
        de: &Direntry,
    ) -> Result<(Entry, Option<Entry>), String> {
        let e = self.dir.entry(v, u64::from(de.entry))?;
        if de.is_dir() != e.is_dir() {
            return Err(String::from(
                "direntry directory bit does not match entry directory bit",
            ));
        }
        if !de.is_dir() {
            return Ok((e, None));
        }
        let me = self.dir.entry(v, u64::from(de.mentry))?;
        Ok((e, Some(me)))
    }

    /// The directory `de` names; `de` must be a directory of this one.
    pub fn child(&mut self, v: &mut dyn Venti, de: &Direntry) -> Result<VacDir, String> {
        match self.open(v, de)? {
            (e, Some(me)) => Ok(VacDir::new(e, me)),
            _ => Err(String::from(purgatorio::abi::error::ENOTDIR)),
        }
    }
}

/// Opens the archive whose root block is `score`: its root directory, the
/// root directory's record and the root block.
pub fn vdroot(v: &mut dyn Venti, score: &Score) -> Result<(VacDir, Direntry, Root), String> {
    let d = v
        .read(score, ROOTTYPE, ROOTSIZE)
        .map_err(|e| format!("reading vac score: {e}"))?;
    let root = Root::unpack(&d).map_err(|e| format!("bad vac root block: {e}"))?;
    if root.rtype != "vac" {
        return Err(String::from("bad vac root block: not a vac root"));
    }
    let mut top = Source::new(Entry {
        generation: 0,
        psize: psize(root.blocksize),
        dsize: root.blocksize,
        depth: 0,
        flags: ENTRYACTIVE | ENTRYDIR,
        size: 3 * ENTRYSIZE as u64,
        score: root.score,
    });
    let mut es = top_entries(v, &mut top);
    if es.is_err() {
        // Fossil's archives have one more level: the root's block holds
        // one entry, for a stream holding the three.
        let e0 = top
            .entry(v, 0)
            .map_err(|e| format!("reading rootdir score: {e}"))?;
        es = top_entries(v, &mut Source::new(e0));
    }
    let [e0, e1, e2] = es.map_err(|e| format!("reading rootdir score: {e}"))?;
    let mut mroot = Source::new(e2);
    let mut b = mroot
        .block(v, 0)
        .map_err(|e| format!("reading root meta entry: {e}"))?;
    b.resize(mroot.e.dsize as usize, 0);
    let de = MetaBlock::parse(&b)
        .and_then(|mb| mb.direntry(0))
        .map_err(|e| format!("reading root meta entry: {e}"))?;
    Ok((VacDir::new(e0, e1), de, root))
}

fn top_entries(v: &mut dyn Venti, top: &mut Source) -> Result<[Entry; 3], String> {
    Ok([top.entry(v, 0)?, top.entry(v, 1)?, top.entry(v, 2)?])
}
