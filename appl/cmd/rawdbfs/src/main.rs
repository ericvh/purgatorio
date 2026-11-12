//! rawdbfs [-abcelx] [-u cmd] file mountpoint: serve the records of the
//! binary database `file` as files on `mountpoint`: `0`, `1`, ... in the
//! order the records have in `file`, and `new`, as dbfs(4) does, but a
//! record may hold any bytes, and a change appends to `file` rather than
//! rewriting it.
//!
//! Reading a numbered file gives its record. A write at offset 0, or on
//! a descriptor opened with `OTRUNC`, starts the record afresh; a write
//! further on puts its bytes there, so a record can be longer than one
//! write. Removing the file deletes the record. Opening `new` makes a
//! record and leaves the descriptor on it; `fstat` then gives its name.
//! Names are positions, so they change from one run to the next after a
//! removal.
//!
//! `-a`, `-b` and `-c` are as for bind(1). `-e` makes `file` when it is
//! not there. `-l` lets only one descriptor at a time have a record open
//! for writing. `-x` adds `index`, which after an integer is written to
//! it reads as a larger integer each time, and `stats`, which reads as
//! the numbers of record reads, writes, creates and deletes. `-u cmd`
//! runs `sh -c cmd` after each change to the database, its output on
//! rawdbfs's standard error.
//!
//! The file is `rawdbfs1\n` and then a log of changes, each an operation
//! byte (`w` a whole record, `p` part of one at an offset, `d` a
//! deletion), the record's number in the file (u32), and for `w` and
//! `p` the offset (u32), the length (u32) and the bytes; integers are
//! little-endian. A torn last entry is dropped, and the file is rewritten
//! without dead entries when they are more than half of it.

use std::collections::BTreeMap;
use std::io::Write;

use purgatorio::abi::mount::MREPL;
use purgatorio::abi::pctl::NEWFD;
use purgatorio::abi::{error, mode};
use purgatorio::arg::Arg;
use purgatorio::styx::{IOHDRSZ, StyxFs};
use purgatorio::{Context, DMDIR, Dir, Error, Qid};

const USAGE: &str = "rawdbfs [-abcelx] [-u cmd] file mountpoint";
const MAGIC: &[u8] = b"rawdbfs1\n";
const QTDIR: u8 = 0x80;
const QROOT: u64 = 0;
const QNEW: u64 = 1;
const QINDEX: u64 = 2;
const QSTATS: u64 = 3;
/// Record `n` has qid path `n + QREC`.
const QREC: u64 = 4;

/// A change, as the log holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Whole(u32, Vec<u8>),
    Part(u32, u32, Vec<u8>),
    Delete(u32),
}

impl Op {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let (tag, id, at, data): (u8, u32, u32, &[u8]) = match self {
            Op::Whole(id, d) => (b'w', *id, 0, d),
            Op::Part(id, at, d) => (b'p', *id, *at, d),
            Op::Delete(id) => (b'd', *id, 0, &[]),
        };
        out.push(tag);
        out.extend_from_slice(&id.to_le_bytes());
        if tag != b'd' {
            out.extend_from_slice(&at.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
        }
        out
    }
}

/// The records a log leaves, in the order they were first written, with
/// their ids, and the length of the log's whole entries.
/// Records with their ids.
type Records = Vec<(u32, Vec<u8>)>;

fn replay(log: &[u8]) -> Result<(Records, usize), String> {
    if log.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(String::from("not a rawdbfs file"));
    }
    let mut at = MAGIC.len();
    let mut order: Vec<u32> = Vec::new();
    let mut recs: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let u32_at =
        |p: usize| -> Option<u32> { Some(u32::from_le_bytes(log.get(p..p + 4)?.try_into().ok()?)) };
    while let Some(&tag) = log.get(at) {
        let Some(id) = u32_at(at + 1) else {
            break;
        };
        if tag == b'd' {
            recs.remove(&id);
            order.retain(|&i| i != id);
            at += 5;
            continue;
        }
        if tag != b'w' && tag != b'p' {
            break;
        }
        let (Some(off), Some(len)) = (u32_at(at + 5), u32_at(at + 9)) else {
            break;
        };
        let start = at + 13;
        let Some(data) = start
            .checked_add(len as usize)
            .and_then(|end| log.get(start..end))
        else {
            break;
        };
        let rec = recs.entry(id).or_insert_with(|| {
            order.push(id);
            Vec::new()
        });
        if tag == b'w' {
            *rec = data.to_vec();
        } else {
            let off = off as usize;
            if rec.len() < off + data.len() {
                rec.resize(off + data.len(), 0);
            }
            rec[off..off + data.len()].copy_from_slice(data);
        }
        at = start + len as usize;
    }
    let out = order
        .into_iter()
        .filter_map(|id| recs.remove(&id).map(|d| (id, d)))
        .collect();
    Ok((out, at))
}

struct Record {
    id: u32,
    data: Vec<u8>,
    vers: u32,
}

#[derive(Clone)]
struct Fid {
    path: u64,
    open: Option<u8>,
    /// Opened with `OTRUNC` and not written yet.
    trunc: bool,
    snap: Vec<u8>,
}

struct Raw {
    file: String,
    user: String,
    exclusive: bool,
    extra: bool,
    update: Option<String>,
    /// By number; `None` once removed.
    records: Vec<Option<Record>>,
    next_id: u32,
    /// The bytes the log holds, and the bytes of live records.
    log_len: usize,
    fids: BTreeMap<u32, Fid>,
    /// The fid writing each record, with `-l`.
    writers: BTreeMap<u64, u32>,
    index: Option<i64>,
    stats: [u64; 4],
    iounit: u32,
}

impl Raw {
    fn load(file: &str) -> Result<Raw, String> {
        let log = std::fs::read(file).map_err(|e| format!("{file}: {}", Error::from(e)))?;
        let log = if log.is_empty() { MAGIC.to_vec() } else { log };
        let (recs, used) = replay(&log).map_err(|e| format!("{file}: {e}"))?;
        let next_id = recs.iter().map(|r| r.0 + 1).max().unwrap_or(0);
        let mut r = Raw {
            file: String::from(file),
            user: purgatorio::auth::user(),
            exclusive: false,
            extra: false,
            update: None,
            records: recs
                .into_iter()
                .map(|(id, data)| Some(Record { id, data, vers: 0 }))
                .collect(),
            next_id,
            log_len: used,
            fids: BTreeMap::new(),
            writers: BTreeMap::new(),
            index: None,
            stats: [0; 4],
            iounit: 8192,
        };
        let live: usize = r.records.iter().flatten().map(|x| x.data.len() + 13).sum();
        // A torn tail, or more dead entries than live ones: write it anew.
        if used != log.len() || used - MAGIC.len() > 2 * live + 64 || log.len() == MAGIC.len() {
            r.compact()?;
        }
        Ok(r)
    }

    fn compact(&mut self) -> Result<(), String> {
        let mut out = MAGIC.to_vec();
        for rec in self.records.iter().flatten() {
            out.extend(Op::Whole(rec.id, rec.data.clone()).encode());
        }
        std::fs::write(&self.file, &out).map_err(|e| Error::from(e).0)?;
        self.log_len = out.len();
        Ok(())
    }

    /// Appends `op` to the file and runs the `-u` command.
    fn log(&mut self, op: Op) -> Result<(), String> {
        let bytes = op.encode();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.file)
            .map_err(|e| Error::from(e).0)?;
        f.write_all(&bytes).map_err(|e| Error::from(e).0)?;
        self.log_len += bytes.len();
        if let Some(cmd) = &self.update {
            // Not fd 0, which is this server's conversation.
            let null = purgatorio::open("/dev/null", mode::OREAD).map_err(|e| e.0)?;
            let fds = [(std::os::fd::AsRawFd::as_raw_fd(&null), 0), (2, 1), (2, 2)];
            let argv = [String::from("sh"), String::from("-c"), cmd.clone()];
            if let Ok(pid) = purgatorio::spawn("/bin/sh.wasm", &argv, NEWFD, &fds) {
                while let Ok(w) = purgatorio::wait() {
                    if w.pid == pid {
                        break;
                    }
                }
            }
        }
        Ok(())
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

    fn stats_text(&self) -> String {
        let [r, w, c, d] = self.stats;
        format!("{r} {w} {c} {d}\n")
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, mode, length) = match path {
            QROOT => (String::from("/"), DMDIR | 0o777, 0),
            QNEW => (String::from("new"), 0o666, 0),
            QINDEX => (String::from("index"), 0o666, 0),
            QSTATS => (String::from("stats"), 0o444, self.stats_text().len() as u64),
            p => (
                (p - QREC).to_string(),
                0o666,
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
        let mut fixed = vec![QNEW];
        if self.extra {
            fixed.extend([QINDEX, QSTATS]);
        }
        for p in fixed {
            let _ = self.dir(p).pack_into(&mut out);
        }
        for (n, r) in self.records.iter().enumerate() {
            if r.is_some() {
                let _ = self.dir(n as u64 + QREC).pack_into(&mut out);
            }
        }
        out
    }

    fn release(&mut self, fid: u32) {
        self.writers.retain(|_, f| *f != fid);
    }
}

fn slice(data: &[u8], offset: u64, count: u32) -> Vec<u8> {
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(data.len());
    let end = start.saturating_add(count as usize).min(data.len());
    data[start..end].to_vec()
}

impl StyxFs for Raw {
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
                open: None,
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
                (QROOT, "index") if self.extra => Some(QINDEX),
                (QROOT, "stats") if self.extra => Some(QSTATS),
                (QROOT, n) => n
                    .parse::<u64>()
                    .ok()
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
                open: None,
                trunc: false,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let mut path = self.fid(fid)?.path;
        let writing = omode & 3 != mode::OREAD || omode & (mode::OTRUNC | mode::ORCLOSE) != 0;
        if (path == QROOT || path == QSTATS) && writing {
            return Err(String::from(error::EPERM));
        }
        if path == QNEW {
            // The descriptor is on the record made, not on `new`.
            let id = self.next_id;
            self.next_id += 1;
            self.records.push(Some(Record {
                id,
                data: Vec::new(),
                vers: 0,
            }));
            path = self.records.len() as u64 - 1 + QREC;
            self.stats[2] += 1;
            self.log(Op::Whole(id, Vec::new()))?;
        } else if path >= QREC {
            self.record(path)?;
        }
        if path >= QREC && writing && self.exclusive {
            match self.writers.get(&path) {
                Some(&f) if f != fid => return Err(String::from(error::EINUSE)),
                _ => {
                    self.writers.insert(path, fid);
                }
            }
        }
        if let Some(f) = self.fids.get_mut(&fid) {
            f.path = path;
            f.open = Some(omode);
            f.trunc = path >= QREC && omode & mode::OTRUNC != 0;
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
        if f.open.is_none() {
            return Err(String::from("fid not open"));
        }
        match f.path {
            QROOT => {
                if offset == 0 {
                    let snap = self.listing();
                    if let Some(f) = self.fids.get_mut(&fid) {
                        f.snap = snap;
                    }
                }
                Ok(styxserv::rotree::dir_slice(
                    &self.fid(fid)?.snap,
                    offset,
                    count,
                ))
            }
            QINDEX => {
                let Some(n) = self.index.as_mut() else {
                    return Ok(Vec::new());
                };
                if offset != 0 {
                    return Ok(Vec::new());
                }
                *n += 1;
                Ok(slice(format!("{n}\n").as_bytes(), 0, count))
            }
            QSTATS => Ok(slice(self.stats_text().as_bytes(), offset, count)),
            QNEW => Ok(Vec::new()),
            p => {
                let data = slice(&self.record(p)?.data, offset, count);
                if offset == 0 {
                    self.stats[0] += 1;
                }
                Ok(data)
            }
        }
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let f = self.fid(fid)?;
        let (path, trunc) = (f.path, f.trunc);
        if !f
            .open
            .is_some_and(|m| matches!(m & 3, mode::OWRITE | mode::ORDWR))
        {
            return Err(String::from(error::EPERM));
        }
        if path == QINDEX {
            let text = String::from_utf8_lossy(data);
            let n = text
                .trim()
                .parse::<i64>()
                .map_err(|_| String::from("bad index"))?;
            self.index = Some(n);
            return Ok(data.len() as u32);
        }
        if path < QREC {
            return Err(String::from(error::EPERM));
        }
        let off = u32::try_from(offset).map_err(|_| String::from("offset too big"))?;
        let rec = self.record_mut(path)?;
        let id = rec.id;
        let op = if off == 0 || trunc {
            rec.data.clear();
            rec.data.resize(off as usize, 0);
            rec.data.extend_from_slice(data);
            if off == 0 {
                Op::Whole(id, data.to_vec())
            } else {
                Op::Whole(id, rec.data.clone())
            }
        } else {
            let end = off as usize + data.len();
            if rec.data.len() < end {
                rec.data.resize(end, 0);
            }
            rec.data[off as usize..end].copy_from_slice(data);
            Op::Part(id, off, data.to_vec())
        };
        rec.vers = rec.vers.wrapping_add(1);
        if let Some(f) = self.fids.get_mut(&fid) {
            f.trunc = false;
        }
        self.stats[1] += 1;
        self.log(op)?;
        Ok(data.len() as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        self.release(fid);
        if f.trunc && self.record(f.path).is_ok_and(|r| !r.data.is_empty()) {
            let rec = self.record_mut(f.path)?;
            rec.data.clear();
            rec.vers = rec.vers.wrapping_add(1);
            let id = rec.id;
            self.stats[1] += 1;
            self.log(Op::Whole(id, Vec::new()))?;
        }
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let path = self.fid(fid)?.path;
        self.fids.remove(&fid);
        self.release(fid);
        if path < QREC {
            return Err(String::from(error::EPERM));
        }
        let id = self.record(path)?.id;
        self.records[(path - QREC) as usize] = None;
        self.stats[3] += 1;
        self.log(Op::Delete(id))
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
        let [_, _, flags, update, file] = args.as_slice() else {
            return Err(Error::from("bad --serve arguments"));
        };
        let mut r = Raw::load(file).map_err(Error)?;
        r.exclusive = flags.contains('l');
        r.extra = flags.contains('x');
        r.update = update.strip_prefix('+').map(String::from);
        return styxserv::bg::serve(&mut r);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut flags = MREPL;
    let mut make = false;
    let mut opts = String::from("-");
    let mut update = String::from("-");
    while let Some(c) = arg.opt() {
        match c {
            'a' | 'b' | 'c' => {
                styxserv::bg::flag(c, &mut flags);
            }
            'e' => make = true,
            'l' | 'x' => opts.push(c),
            'u' => update = format!("+{}", arg.earg()?),
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
    // Read it here too, so a bad file is this command's error.
    Raw::load(&file).map_err(Error)?;
    let argv = [
        String::from("rawdbfs"),
        String::from("--serve"),
        opts,
        update,
        file,
    ];
    styxserv::bg::mount("/bin/rawdbfs.wasm", &argv, mtpt, flags)
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log() {
        let mut log = MAGIC.to_vec();
        for op in [
            Op::Whole(0, b"zero".to_vec()),
            Op::Whole(1, b"one".to_vec()),
            Op::Part(1, 5, b"\0\nx".to_vec()),
            Op::Whole(2, b"two".to_vec()),
            Op::Delete(0),
            Op::Whole(0, b"back".to_vec()),
        ] {
            log.extend(op.encode());
        }
        let (recs, used) = replay(&log).unwrap();
        assert_eq!(used, log.len());
        assert_eq!(
            recs,
            [
                (1, b"one\0\0\0\nx".to_vec()),
                (2, b"two".to_vec()),
                (0, b"back".to_vec())
            ]
        );
        // A torn tail is left out, wherever the log is cut.
        for cut in MAGIC.len()..log.len() {
            let (_, used) = replay(&log[..cut]).unwrap();
            assert!(used <= cut);
        }
        assert!(replay(b"junk").is_err());
        assert_eq!(replay(MAGIC).unwrap().0, []);
    }
}
