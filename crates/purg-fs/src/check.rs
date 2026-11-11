//! The checker: walks the whole tree from the root and compares what it
//! finds with the free map. It never writes; on a read-only mount the
//! journal's images are seen through the overlay, so a disk that crashed
//! is checked as the next mount would see it.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::Disk;
use crate::fs::{Fs, Ref};
use crate::layout::{DENTRY, DMDIR, Dentry, NDIRECT, Slot, ptr_at, valid_name, valid_user};

/// The most blocks the checker tracks (its bitmap takes an eighth of this
/// in bytes).
const MAX_CHECK: u64 = 1 << 30;
/// The most problems it lists.
const MAX_ERRORS: usize = 100;

/// What [`Fs::check`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Each problem, with the file's path when there is one. Empty for a
    /// consistent file system.
    pub errors: Vec<String>,
    pub files: u64,
    pub dirs: u64,
    /// Blocks in use (metadata included) and free.
    pub used: u64,
    pub free: u64,
    /// The sequence number of a journal the mount replayed.
    pub replayed: Option<u64>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

struct Walk {
    seen: Vec<u8>,
    qids: BTreeSet<u64>,
    report: Report,
}

impl Walk {
    fn err(&mut self, e: String) {
        if self.report.errors.len() < MAX_ERRORS {
            self.report.errors.push(e);
        }
    }

    /// Marks block `b`; false if it was already marked.
    fn mark(&mut self, b: u64) -> bool {
        let (i, bit) = ((b / 8) as usize, b % 8);
        let was = self.seen[i] & (1 << bit) != 0;
        self.seen[i] |= 1 << bit;
        !was
    }

    fn marked(&self, b: u64) -> bool {
        self.seen[(b / 8) as usize] & (1 << (b % 8)) != 0
    }
}

impl<D: Disk> Fs<D> {
    /// Checks the whole file system; see [`Report`].
    pub fn check(&mut self) -> Report {
        let n = self.sb.nblocks;
        let mut w = Walk {
            seen: vec![0u8; n.min(MAX_CHECK).div_ceil(8) as usize],
            qids: BTreeSet::new(),
            report: Report {
                replayed: self.replayed,
                ..Report::default()
            },
        };
        if n > MAX_CHECK {
            w.err(format!("{n} blocks: too many to check"));
            return w.report;
        }
        for b in 0..self.sb.dstart {
            w.mark(b);
        }
        let root = self.root().me;
        let mut stack: Vec<(Ref, String)> = Vec::new();
        match self.dentry(root) {
            Ok(d) => {
                w.qids.insert(d.qpath);
                if d.qpath >= self.sb.qidgen {
                    w.err(format!(
                        "/: qid path {} not below {}",
                        d.qpath, self.sb.qidgen
                    ));
                }
                stack.push((root, String::new()));
            }
            Err(e) => w.err(format!("/: {e}")),
        }
        while let Some((r, path)) = stack.pop() {
            let Ok(mut d) = self.dentry(r) else {
                w.err(format!("{path}/: cannot read the entry"));
                continue;
            };
            w.report.dirs += 1;
            let blocks = self.blocks_of(&mut d, &path, &mut w, true);
            let mut names = BTreeSet::new();
            for b in blocks {
                let buf = match self.meta(b) {
                    Ok(buf) => buf.to_vec(),
                    Err(e) => {
                        w.err(format!("{path}/: block {b}: {e}"));
                        continue;
                    }
                };
                for s in 0..self.bs / DENTRY {
                    let e = match Dentry::decode(&buf[s * DENTRY..(s + 1) * DENTRY]) {
                        Slot::Free => continue,
                        Slot::Bad(why) => {
                            w.err(format!("{path}/: block {b} slot {s}: {why}"));
                            continue;
                        }
                        Slot::Used(e) => e,
                    };
                    let p = format!("{path}/{}", e.name);
                    if !valid_name(&e.name) {
                        w.err(format!("{p}: bad name"));
                    }
                    if !names.insert(e.name.clone()) {
                        w.err(format!("{p}: name appears twice"));
                    }
                    if !valid_user(&e.uid) || !valid_user(&e.gid) {
                        w.err(format!("{p}: bad owner or group"));
                    }
                    if !w.qids.insert(e.qpath) {
                        w.err(format!("{p}: qid path {} used twice", e.qpath));
                    }
                    if e.qpath >= self.sb.qidgen {
                        w.err(format!(
                            "{p}: qid path {} not below {}",
                            e.qpath, self.sb.qidgen
                        ));
                    }
                    let me = Ref {
                        block: b as u32,
                        slot: s as u16,
                        path: e.qpath,
                    };
                    if e.mode & DMDIR != 0 {
                        stack.push((me, p));
                    } else {
                        w.report.files += 1;
                        let mut e = e;
                        self.blocks_of(&mut e, &p, &mut w, false);
                    }
                }
            }
        }
        self.check_map(&mut w);
        w.report
    }

    /// Marks the blocks of `d` (pointer blocks too) and returns its data
    /// blocks, in order.
    fn blocks_of(&mut self, d: &mut Dentry, path: &str, w: &mut Walk, dir: bool) -> Vec<u64> {
        let bs = self.bs as u64;
        let p = bs / 4;
        let name = if path.is_empty() { "/" } else { path };
        if dir && !d.length.is_multiple_of(bs) {
            w.err(format!(
                "{name}: directory length {} is not whole blocks",
                d.length
            ));
        }
        let nblocks = d.length.div_ceil(bs);
        let mut data = Vec::new();
        let claim = |fs: &Self, w: &mut Walk, b: u32, what: &str| -> bool {
            if fs.check_data(b).is_err() {
                w.err(format!("{name}: {what} pointer {b} out of range"));
                return false;
            }
            if !w.mark(u64::from(b)) {
                w.err(format!("{name}: block {b} used twice"));
                return false;
            }
            true
        };
        let leaf = |fs: &Self, w: &mut Walk, data: &mut Vec<u64>, idx: u64, b: u32| {
            if b == 0 {
                return;
            }
            if idx >= nblocks {
                w.err(format!("{name}: block {b} past the end"));
            }
            if claim(fs, w, b, "data") {
                data.push(u64::from(b));
            }
        };
        for i in 0..NDIRECT {
            leaf(self, w, &mut data, i as u64, d.ptrs[i]);
        }
        let ind = d.ptrs[NDIRECT];
        if ind != 0 && claim(self, w, ind, "indirect") {
            match self.meta(u64::from(ind)).map(|b| b.to_vec()) {
                Ok(buf) => {
                    for i in 0..p {
                        leaf(
                            self,
                            w,
                            &mut data,
                            NDIRECT as u64 + i,
                            ptr_at(&buf, i as usize),
                        );
                    }
                }
                Err(e) => w.err(format!("{name}: block {ind}: {e}")),
            }
        }
        let dind = d.ptrs[NDIRECT + 1];
        if dind != 0 && claim(self, w, dind, "double indirect") {
            match self.meta(u64::from(dind)).map(|b| b.to_vec()) {
                Ok(top) => {
                    for j in 0..p {
                        let l1 = ptr_at(&top, j as usize);
                        if l1 == 0 || !claim(self, w, l1, "indirect") {
                            continue;
                        }
                        match self.meta(u64::from(l1)).map(|b| b.to_vec()) {
                            Ok(buf) => {
                                for i in 0..p {
                                    let idx = NDIRECT as u64 + p + j * p + i;
                                    leaf(self, w, &mut data, idx, ptr_at(&buf, i as usize));
                                }
                            }
                            Err(e) => w.err(format!("{name}: block {l1}: {e}")),
                        }
                    }
                }
                Err(e) => w.err(format!("{name}: block {dind}: {e}")),
            }
        }
        data
    }

    /// Compares the blocks the walk found with the free map.
    fn check_map(&mut self, w: &mut Walk) {
        let per = 8 * self.bs as u64;
        let n = self.sb.nblocks;
        for k in 0..u64::from(self.sb.blen) {
            let map = match self.meta(self.sb.bstart + k) {
                Ok(m) => m.to_vec(),
                Err(e) => {
                    w.err(format!("free map block {k}: {e}"));
                    continue;
                }
            };
            for bit in 0..per {
                let b = k * per + bit;
                let set = map[(bit / 8) as usize] & (1 << (bit % 8)) != 0;
                if b >= n {
                    if !set {
                        w.err(format!("free map: block {b} past the end is free"));
                    }
                    continue;
                }
                match (set, w.marked(b)) {
                    (true, true) => w.report.used += 1,
                    (false, false) => w.report.free += 1,
                    (false, true) => {
                        w.report.used += 1;
                        w.err(format!("block {b} is in use but marked free"));
                    }
                    (true, false) => {
                        w.report.used += 1;
                        w.err(format!("block {b} is marked in use but nothing uses it"));
                    }
                }
            }
        }
    }
}
