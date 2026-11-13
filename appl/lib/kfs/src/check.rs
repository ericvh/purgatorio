//! Checking a kfs (kfscmd(8)'s `check`): walk the tree from the root,
//! marking every block and qid path once, then the free list, and report
//! what is wrong in kfs's words.

use super::*;

/// The letters `check` takes.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckFlags {
    /// `r`: read every data block and check its tag.
    pub rdall: bool,
    /// `t`: fix bad tags.
    pub tag: bool,
    /// `P`: print every file's name.
    pub pfile: bool,
    /// `p`: print every directory's name.
    pub pdir: bool,
    /// `f`: rebuild the free list.
    pub free: bool,
    /// `c`: fix bad tags and clear the block.
    pub ream: bool,
    /// `d`: remove second references to a block.
    pub bad: bool,
    /// `q`: errors only.
    pub quiet: bool,
}

/// Parses `check`'s letters. `w` (write every block touched) and `v` are
/// accepted and change nothing: blocks are written as they change.
pub fn check_flags(s: &str) -> Option<CheckFlags> {
    let mut f = CheckFlags::default();
    for c in s.chars() {
        match c {
            'r' => f.rdall = true,
            't' => f.tag = true,
            'P' => f.pfile = true,
            'p' => f.pdir = true,
            'f' => f.free = true,
            'c' => f.ream = true,
            'd' => f.bad = true,
            'q' => f.quiet = true,
            'w' | 'v' => {}
            _ => return None,
        }
    }
    Some(f)
}

struct Map {
    lo: u32,
    hi: u32,
    bits: Vec<u8>,
    nbad: u32,
    ndup: u32,
    nmark: u32,
}

impl Map {
    fn new(lo: u32, hi: u32) -> Map {
        Map {
            lo,
            hi,
            bits: vec![0; ((hi - lo) as usize).div_ceil(8)],
            nbad: 0,
            ndup: 0,
            nmark: 0,
        }
    }

    fn is_set(&self, a: u32) -> bool {
        if a < self.lo || a >= self.hi {
            return false;
        }
        let i = (a - self.lo) as usize;
        self.bits[i >> 3] & (1 << (i & 7)) != 0
    }

    fn mark(&mut self, a: u32) -> Option<&'static str> {
        if a < self.lo || a >= self.hi {
            self.nbad += 1;
            return Some("out of range");
        }
        let i = (a - self.lo) as usize;
        let b = 1 << (i & 7);
        if self.bits[i >> 3] & b != 0 {
            self.ndup += 1;
            return Some("dup");
        }
        self.bits[i >> 3] |= b;
        self.nmark += 1;
        None
    }
}

const MAXDEPTH: u32 = 100;
const MAXNAME: usize = 4000;

struct Check<'a, D: Dev> {
    fs: &'a mut Fs<D>,
    f: CheckFlags,
    amap: Map,
    qmap: Map,
    name: String,
    nfiles: u32,
    maxq: u32,
    modified: u32,
    out: Vec<String>,
}

/// Checks `fs` and returns the report, one line each.
pub fn check<D: Dev>(fs: &mut Fs<D>, f: CheckFlags) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(blk) = fs.read_block(SUPERADDR) else {
        out.push(String::from("bad superblock"));
        return out;
    };
    if !tag_ok(&blk, fs.bsize, TSUPER, QPSUPER) {
        out.push(String::from("bad superblock"));
        return out;
    }
    let sb = fs.superblock().clone();
    if sb.fstart != 1 {
        out.push(String::from("invalid superblock"));
        return out;
    }
    if sb.fsize < sb.fstart || sb.fsize > fs.nblocks {
        out.push(String::from("invalid size in superblock"));
        return out;
    }
    let nqid = (sb.qidgen.saturating_add(100)).clamp(64 * 1024, 8 * 1024 * 1024);
    let mut c = Check {
        fs,
        f,
        amap: Map::new(sb.fstart, sb.fsize),
        qmap: Map::new(0, nqid),
        name: String::new(),
        nfiles: 0,
        maxq: 0,
        modified: 0,
        out,
    };
    c.amark(SUPERADDR);
    if !f.quiet {
        c.out.push(String::from("checking file system: main"));
    }
    if let Some(mut d) = c.maked(ROOTADDR, 0, QPROOT) {
        c.amark(ROOTADDR);
        if c.fsck(&mut d, 1) {
            c.modd(ROOT, &d);
        }
    }
    if sb.qidgen < c.maxq {
        c.out.push(format!(
            "qid generator low path={} maxq={}",
            sb.qidgen, c.maxq
        ));
    }
    let nqbad = c.qmap.nbad + c.qmap.ndup;
    let ndup = c.amap.ndup;
    let nused = c.amap.nmark;
    c.amap.ndup = 0;
    c.amap.nmark = 0;
    if f.free {
        c.name = String::from("free list");
        let amap = &c.amap;
        let used: Vec<bool> = (0..sb.fsize).map(|a| amap.is_set(a)).collect();
        if let Err(e) =
            c.fs.rebuild_free(|a| used.get(a as usize).copied().unwrap_or(true))
        {
            c.out.push(format!("check: free list: {e}"));
        }
        for a in sb.fstart..sb.fsize {
            if !c.amap.is_set(a) {
                c.amap.mark(a);
            }
        }
        let _ = c.fs.set_qidgen(c.maxq);
    } else {
        c.ckfreelist();
    }
    let tfree = c.fs.superblock().tfree;
    let nbad = c.amap.nbad;
    let nfdup = c.amap.ndup;
    let nfree = c.amap.nmark;
    if c.modified != 0 {
        c.out.push(String::from("file system was modified"));
    }
    let total = sb.fsize - sb.fstart;
    if !f.quiet {
        c.out.push(format!("{:8} files", c.nfiles));
        c.out.push(format!("{:8} blocks in the file system", total));
        c.out.push(format!("{:8} used blocks", nused));
        c.out.push(format!("{:8} free blocks", tfree));
    }
    if !f.free {
        if nfree != tfree {
            c.out.push(format!("{:8} free blocks found", nfree));
        }
        if nfdup != 0 {
            c.out
                .push(format!("{:8} blocks duplicated in the free list", nfdup));
        }
        let missing = i64::from(total) - i64::from(nused) - i64::from(nfree);
        if missing != 0 {
            c.out.push(format!("{:8} missing blocks", missing));
        }
    }
    if ndup != 0 {
        c.out.push(format!("{:8} address duplications", ndup));
    }
    if nbad != 0 {
        c.out.push(format!("{:8} bad block addresses", nbad));
    }
    if nqbad != 0 {
        c.out.push(format!("{:8} bad qids", nqbad));
    }
    if !f.quiet {
        c.out.push(format!("{:8} maximum qid path", c.maxq));
    }
    let mut n = 0;
    for a in (sb.fstart..sb.fsize).rev() {
        if !c.amap.is_set(a) {
            c.out.push(format!("missing: {a}"));
            n += 1;
        }
        if n > 10 {
            c.out.push(String::from(" ..."));
            break;
        }
    }
    c.out
}

impl<D: Dev> Check<'_, D> {
    /// Marks a block of the tree. True means drop the reference: it is out
    /// of range. A second reference is reported but kept, since removing
    /// one of the files may be the repair.
    fn amark(&mut self, a: u32) -> bool {
        match self.amap.mark(a) {
            Some(e) => {
                self.out.push(format!("check: \"{}\": {e} {a}", self.name));
                e != "dup"
            }
            None => false,
        }
    }

    /// Marks a block of the free list. True means stop.
    fn fmark(&mut self, a: u32) -> bool {
        match self.amap.mark(a) {
            Some(e) => {
                self.out.push(format!("check: \"{}\": {e} {a}", self.name));
                true
            }
            None => false,
        }
    }

    fn qmark(&mut self, q: u32) {
        if let Some(e) = self.qmap.mark(q)
            && self.qmap.nbad + self.qmap.ndup < 20
        {
            self.out
                .push(format!("check: \"{}\": qid {e} 0x{q:x}", self.name));
        }
    }

    /// Block `a`, if it has the tag; `t` and `c` fix it.
    fn xtag(&mut self, a: u32, tag: u16, path: u32) -> Option<Vec<u8>> {
        if a == 0 {
            return None;
        }
        let bs = self.fs.bsize;
        let fix = self.f.ream || self.f.tag;
        let mut b = match self.fs.read_block(a) {
            Ok(b) => b,
            Err(_) => {
                self.out
                    .push(format!("check: \"{}\": xtag: p null", self.name));
                if !fix {
                    return None;
                }
                vec![0u8; bs]
            }
        };
        if tag_ok(&b, bs, tag, path) {
            return Some(b);
        }
        self.out
            .push(format!("check: \"{}\": xtag: checktag", self.name));
        if !fix {
            return None;
        }
        if self.f.ream {
            b.fill(0);
        }
        set_tag(&mut b, bs, tag, path);
        if self.fs.write_block(a, &b).is_ok() {
            self.modified += 1;
        }
        Some(b)
    }

    fn maked(&mut self, a: u32, slot: u32, path: u32) -> Option<Dentry> {
        let b = self.xtag(a, TDIR, path)?;
        let o = slot as usize * DENTRYSIZE;
        Some(Dentry::unpack(&b[o..o + DENTRYSIZE]))
    }

    /// Writes back an entry whose block pointers were cleared (`d`).
    fn modd(&mut self, loc: Loc, d: &Dentry) {
        if self.f.bad {
            let _ = self.fs.putd(loc, d);
        }
    }

    fn checkdir(&mut self, a: u32, path: u32, depth: u32) {
        let ns = self.name.len();
        for slot in 0..self.fs.dirperbuf() {
            let Some(mut d) = self.maked(a, slot, path) else {
                break;
            };
            if self.fsck(&mut d, depth + 1) {
                self.modd(Loc { addr: a, slot }, &d);
            }
            self.name.truncate(ns);
        }
    }

    fn checkindir(&mut self, a: u32, dir: bool, path: u32, depth: u32) {
        let Some(mut b) = self.xtag(a, TIND1, path) else {
            return;
        };
        let mut changed = false;
        for i in 0..self.fs.indperbuf() as usize {
            let x = get4(&b, 4 * i);
            if x == 0 {
                continue;
            }
            if self.amark(x) {
                if self.f.bad {
                    put4(&mut b, 4 * i, 0);
                    changed = true;
                }
                continue;
            }
            if dir {
                self.checkdir(x, path, depth);
            } else if self.f.rdall {
                self.xtag(x, TFILE, path);
            }
        }
        if changed {
            let _ = self.fs.write_block(a, &b);
        }
    }

    /// Checks one entry and what it holds. True if `d` changed.
    fn fsck(&mut self, d: &mut Dentry, depth: u32) -> bool {
        if depth >= MAXDEPTH {
            self.out.push(format!("max depth exceeded: {}", self.name));
            return false;
        }
        if !d.allocated() {
            return false;
        }
        self.nfiles += 1;
        if d.name.len() >= NAMELEN {
            self.out
                .push(format!("{}.name ({}) not terminated", self.name, d.name));
            return false;
        }
        if self.name.len() + d.name.len() >= MAXNAME {
            self.out
                .push(format!("{}.name ({}) name too large", self.name, d.name));
            return false;
        }
        self.name.push_str(&d.name);
        if d.is_dir() {
            if self.name.len() > 1 {
                self.name.push('/');
            }
            if self.f.pdir {
                self.out.push(self.name.clone());
            }
        } else if self.f.pfile {
            self.out.push(self.name.clone());
        }
        let path = d.path;
        self.qmark(path);
        self.maxq = self.maxq.max(path);
        let mut dmod = false;
        let dir = d.is_dir();
        for i in 0..NDBLOCK {
            let a = d.dblock[i];
            if a == 0 {
                continue;
            }
            if self.amark(a) {
                d.dblock[i] = 0;
                dmod = true;
                continue;
            }
            if dir {
                self.checkdir(a, path, depth);
            } else if self.f.rdall {
                self.xtag(a, TFILE, path);
            }
        }
        if d.iblock != 0 {
            if self.amark(d.iblock) {
                d.iblock = 0;
                dmod = true;
            } else {
                self.checkindir(d.iblock, dir, path, depth);
            }
        }
        if d.diblock != 0 {
            if self.amark(d.diblock) {
                d.diblock = 0;
                return true;
            }
            if let Some(mut b) = self.xtag(d.diblock, TIND2, path) {
                let mut changed = false;
                for i in 0..self.fs.indperbuf() as usize {
                    let x = get4(&b, 4 * i);
                    if x == 0 {
                        continue;
                    }
                    if self.amark(x) {
                        if self.f.bad {
                            put4(&mut b, 4 * i, 0);
                            changed = true;
                        }
                        continue;
                    }
                    self.checkindir(x, dir, path, depth);
                }
                if changed {
                    let _ = self.fs.write_block(d.diblock, &b);
                }
            }
        }
        dmod
    }

    fn ckfreelist(&mut self) {
        self.name = String::from("free list");
        self.out.push(format!("check {}", self.name));
        let fe = self.fs.feperbuf();
        let mut head = self.fs.superblock().blk[SUPER1SIZE..].to_vec();
        let (mut lo, mut hi) = (0u32, 0u32);
        let see = |a: u32, lo: &mut u32, hi: &mut u32| {
            if *lo == 0 || *lo > a {
                *lo = a;
            }
            if *hi == 0 || *hi < a {
                *hi = a;
            }
        };
        loop {
            let n = get4(&head, 0) as usize;
            if n > fe {
                self.out.push(format!("check: nfree bad {n}"));
                break;
            }
            for i in 1..n {
                let a = get4(&head, 4 + 4 * i);
                if a != 0 && !self.fmark(a) {
                    see(a, &mut lo, &mut hi);
                }
            }
            let a = get4(&head, 4);
            if a == 0 || self.fmark(a) {
                break;
            }
            see(a, &mut lo, &mut hi);
            match self.xtag(a, TFREE, QPNONE) {
                Some(b) => head = b,
                None => break,
            }
        }
        self.out.push(format!("lo = {lo}; hi = {hi}"));
    }
}
