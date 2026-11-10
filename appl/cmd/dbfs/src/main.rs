//! dbfs [-abcer] file mountpoint: serve the records of the text database
//! `file` as files on `mountpoint`: `0`, `1`, ... in the order the
//! records have in `file`, and `new`.
//!
//! Reading a numbered file gives its record. A write replaces the whole
//! record with the bytes written, wherever the write says they go.
//! Removing the file deletes the record. Opening `new` makes a record
//! and leaves the descriptor on it; `fstat` then gives its name. Each
//! change rewrites `file`.
//!
//! In `file`, records are separated by an empty line, so a record cannot
//! hold one, and a record that does not end in a newline gains one when
//! `file` is next read. An empty record is not kept, and since names are
//! positions they change from one run to the next after a removal.
//!
//! `-a`, `-b` and `-c` are as for bind(1). `-e` makes `file` when it is
//! not there. `-r` (which dbfs(4) lists without a meaning) serves the
//! records read-only here. Not here: `rawdbfs`.

use std::collections::BTreeMap;

use purgatorio::abi::mount::MREPL;
use purgatorio::abi::{error, mode};
use purgatorio::arg::Arg;
use purgatorio::styx::{IOHDRSZ, StyxFs};
use purgatorio::{Context, DMDIR, Dir, Error, Qid};

const USAGE: &str = "dbfs [-abcer] file mountpoint";
const QTDIR: u8 = 0x80;
const QROOT: u64 = 0;
const QNEW: u64 = 1;
/// Record `n` has qid path `n + QREC`.
const QREC: u64 = 2;

/// The records of a database file's text.
fn parse(text: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for line in text.split_inclusive(|&b| b == b'\n') {
        if line == b"\n" {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.extend_from_slice(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    for r in &mut out {
        if r.last() != Some(&b'\n') {
            r.push(b'\n');
        }
    }
    out
}

/// The text of a database file holding `records`.
fn unparse<'a>(records: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records.filter(|r| !r.is_empty()) {
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(r);
        if r.last() != Some(&b'\n') {
            out.push(b'\n');
        }
    }
    out
}

struct Record {
    data: Vec<u8>,
    vers: u32,
}

#[derive(Clone)]
struct Fid {
    path: u64,
    open: bool,
    /// Opened with `OTRUNC` and not written yet: the record is emptied
    /// when the fid goes, unless a write has replaced it by then.
    trunc: bool,
    /// The packed listing a read of the root returns, made at offset 0.
    snap: Vec<u8>,
}

struct Dbfs {
    file: String,
    rdonly: bool,
    user: String,
    /// By number; `None` once removed.
    records: Vec<Option<Record>>,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

impl Dbfs {
    fn load(file: &str, rdonly: bool) -> Result<Dbfs, Error> {
        let text = std::fs::read(file).map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
        Ok(Dbfs {
            file: String::from(file),
            rdonly,
            user: purgatorio::auth::user(),
            records: parse(&text)
                .into_iter()
                .map(|data| Some(Record { data, vers: 0 }))
                .collect(),
            fids: BTreeMap::new(),
            iounit: 8192,
        })
    }

    fn save(&self) -> Result<(), String> {
        let text = unparse(self.records.iter().flatten().map(|r| r.data.as_slice()));
        std::fs::write(&self.file, text).map_err(|e| Error::from(e).0)
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn record(&self, path: u64) -> Result<&Record, String> {
        path.checked_sub(QREC)
            .and_then(|n| self.records.get(n as usize))
            .and_then(Option::as_ref)
            .ok_or_else(|| String::from(error::ENONEXIST))
    }

    fn record_mut(&mut self, path: u64) -> Result<&mut Record, String> {
        path.checked_sub(QREC)
            .and_then(|n| self.records.get_mut(n as usize))
            .and_then(Option::as_mut)
            .ok_or_else(|| String::from(error::ENONEXIST))
    }

    fn qid(&self, path: u64) -> Qid {
        Qid {
            path,
            vers: self.record(path).map_or(0, |r| r.vers),
            qtype: if path == QROOT { QTDIR } else { 0 },
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let perm = if self.rdonly { 0o444 } else { 0o666 };
        let (name, mode, length) = match path {
            QROOT => (String::from("/"), DMDIR | 0o555 | (perm & 0o222), 0),
            QNEW => (String::from("new"), perm, 0),
            p => (
                (p - QREC).to_string(),
                perm,
                self.record(p).map_or(0, |r| r.data.len() as u64),
            ),
        };
        Dir {
            qid: self.qid(path),
            mode,
            length,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            muid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn listing(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let _ = self.dir(QNEW).pack_into(&mut out);
        for (n, r) in self.records.iter().enumerate() {
            if r.is_some() {
                let _ = self.dir(n as u64 + QREC).pack_into(&mut out);
            }
        }
        out
    }

    fn changed(&mut self, path: u64) -> Result<(), String> {
        if let Ok(r) = self.record_mut(path) {
            r.vers = r.vers.wrapping_add(1);
        }
        self.save()
    }
}

impl StyxFs for Dbfs {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                path: QROOT,
                open: false,
                trunc: false,
                snap: Vec::new(),
            },
        );
        Ok(self.qid(QROOT))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let mut at = self.fid(fid)?.path;
        let mut qids = Vec::new();
        for name in names {
            let next = match (at, *name) {
                (QROOT, "..") => Some(QROOT),
                (QROOT, "new") => Some(QNEW),
                (QROOT, n) => n
                    .parse::<u64>()
                    .ok()
                    // The name is the number as it is listed, and no other spelling of it.
                    .filter(|v| v.to_string() == n)
                    .map(|v| v + QREC)
                    .filter(|&p| self.record(p).is_ok()),
                (_, "..") => Some(QROOT),
                _ => None,
            };
            match next {
                Some(p) => {
                    at = p;
                    qids.push(self.qid(p));
                }
                None if qids.is_empty() => return Err(String::from(error::ENONEXIST)),
                None => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                path: at,
                open: false,
                trunc: false,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let mut path = self.fid(fid)?.path;
        let writing = omode & 3 != mode::OREAD || omode & (mode::OTRUNC | mode::ORCLOSE) != 0;
        if path == QROOT && writing {
            return Err(String::from(error::EPERM));
        }
        if self.rdonly && (writing || path == QNEW) {
            return Err(String::from(error::EPERM));
        }
        if path == QNEW {
            // The descriptor is on the record made, not on `new`.
            self.records.push(Some(Record {
                data: Vec::new(),
                vers: 0,
            }));
            path = self.records.len() as u64 - 1 + QREC;
        } else if path != QROOT {
            self.record(path)?;
        }
        if let Some(f) = self.fids.get_mut(&fid) {
            f.path = path;
            f.open = true;
            f.trunc = path != QROOT && omode & mode::OTRUNC != 0;
        }
        Ok((self.qid(path), self.iounit))
    }

    fn create(
        &mut self,
        _fid: u32,
        _name: &str,
        _perm: u32,
        _mode: u8,
    ) -> Result<(Qid, u32), String> {
        Err(String::from(error::EPERM))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        if !f.open {
            return Err(String::from("fid not open"));
        }
        if f.path != QROOT {
            let data = &self.record(f.path)?.data;
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let end = start.saturating_add(count as usize).min(data.len());
            return Ok(data[start..end].to_vec());
        }
        if offset == 0 {
            let snap = self.listing();
            if let Some(f) = self.fids.get_mut(&fid) {
                f.snap = snap;
            }
        }
        let snap = &self.fid(fid)?.snap;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(snap.len());
        let end = start.saturating_add(count as usize).min(snap.len());
        // Whole entries only.
        let mut n = start;
        while n + 2 <= end {
            let len = u16::from_le_bytes([snap[n], snap[n + 1]]) as usize + 2;
            if n + len > end {
                break;
            }
            n += len;
        }
        Ok(snap[start..n].to_vec())
    }

    fn write(&mut self, fid: u32, _offset: u64, data: &[u8]) -> Result<u32, String> {
        let f = self.fid(fid)?;
        let path = f.path;
        if !f.open || path == QROOT || self.rdonly {
            return Err(String::from(error::EPERM));
        }
        // The write is the record, or, refused, leaves the record alone.
        if let Some(f) = self.fids.get_mut(&fid) {
            f.trunc = false;
        }
        // A blank line would split the record when the file is read back.
        if data.starts_with(b"\n") || data.windows(2).any(|w| w == b"\n\n") {
            return Err(String::from("empty line in record"));
        }
        self.record_mut(path)?.data = data.to_vec();
        self.changed(path)?;
        Ok(data.len() as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if f.trunc && self.record(f.path).is_ok_and(|r| !r.data.is_empty()) {
            self.record_mut(f.path)?.data.clear();
            self.changed(f.path)?;
        }
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let path = self.fid(fid)?.path;
        self.fids.remove(&fid);
        if path < QREC || self.rdonly {
            return Err(String::from(error::EPERM));
        }
        self.record(path)?;
        self.records[(path - QREC) as usize] = None;
        self.save()
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let path = self.fid(fid)?.path;
        if path >= QREC {
            self.record(path)?;
        }
        self.dir(path).pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let rdonly = args.get(2).map(String::as_str) == Some("-r");
        let file = args.last().cloned().unwrap_or_default();
        return styxserv::bg::serve(&mut Dbfs::load(&file, rdonly)?);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut flags = MREPL;
    let (mut make, mut rdonly) = (false, false);
    while let Some(c) = arg.opt() {
        match c {
            'a' | 'b' | 'c' => {
                styxserv::bg::flag(c, &mut flags);
            }
            'e' => make = true,
            'r' => rdonly = true,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [file, mtpt] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let file = styxserv::abs(file);
    if make && purgatorio::stat(&file).is_err() {
        purgatorio::create(&file, mode::OWRITE, 0o666)
            .map_err(|e| Error(format!("cannot create {file}: {e}")))?;
    }
    // Read it here too, so a missing file is this command's error.
    Dbfs::load(&file, rdonly)?;
    let mut argv = vec![String::from("dbfs"), String::from("--serve")];
    if rdonly {
        argv.push(String::from("-r"));
    }
    argv.push(file);
    styxserv::bg::mount("/bin/dbfs.wasm", &argv, mtpt, flags)
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text() {
        let recs = parse(b"\n\none\ntwo\n\n\nthree\n\nlast");
        assert_eq!(recs, [&b"one\ntwo\n"[..], b"three\n", b"last\n"]);
        let text = unparse(recs.iter().map(Vec::as_slice));
        assert_eq!(text, b"one\ntwo\n\nthree\n\nlast\n");
        assert_eq!(parse(&text), recs);
        assert!(parse(b"").is_empty());
        assert!(parse(b"\n\n").is_empty());
        let odd: [&[u8]; 3] = [b"", b"x", b"y\n"];
        assert_eq!(unparse(odd.into_iter()), b"x\n\ny\n");
    }
}
