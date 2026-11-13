//! disk/kfs [-r [-b bsize]] [-cADPRW] [-n name] file: serve the kfs file
//! system in `file` (an image or a disk partition) on standard input, as
//! kfs(4): `mount -c {disk/kfs file} dir`. The format is Plan 9's kfs
//! with Inferno's modifying user (`appl/lib/kfs`, `docs/fs.md`), so images
//! move between this, Inferno and Plan 9.
//!
//! - `-r` reams: makes an empty file system with `bsize`-byte blocks
//!   (`-b`, default 1024, a multiple of 512 up to 16384) over the whole
//!   file, and turns permission checking off (`-P` and `-W`).
//! - Otherwise the file system is checked, and its free list rebuilt,
//!   when it was not shut down cleanly, unless `-c` is given.
//! - `-n name` serves a console, `/chan/kfs.<name>.cmd`, for
//!   `disk/kfscmd`.
//! - `-A` does not update access times, `-P` turns off permission
//!   checking, `-R` serves the file system read only, `-W` lets wstat
//!   change owners and groups freely. `-D` is accepted and ignored.
//!
//! Users are named in the file system's own `adm/users` (users(6)); a
//! name not in it is `none`. Messages go to standard error, since
//! standard output is the 9P conversation under `mount {...}`. Errors
//! are kfs's own (`create -- file exists`, ...).
//!
//! Where it differs from Inferno's: an exclusive-use file stays locked
//! while it is open, not for five minutes; the console takes commands
//! from any user who can open it (see `#s`); atimes are set by attach,
//! read, write and wstat, not by walks; `check`'s `w` flag does nothing,
//! since every change is written at once.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::FromRawFd;

use kfs::*;
use purg_styx::{
    DMAPPEND, DMDIR, DMEXCL, Dir, IOHDRSZ, QTAPPEND, QTDIR, QTEXCL, QTFILE, Qid, StyxFs,
};
use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::file2chan::{self, Event, FileIO, Request};
use purgatorio::{Context, Error, daytime};

const USAGE: &str = "disk/kfs [-r [-b bufsize]] [-cADPRW] [-n name] kfsfile";

/// Inferno's MAXRPC: the largest message kfs offers.
const MAXRPC: u32 = 8192 + IOHDRSZ;

const FREAD: u8 = 1;
const FWRITE: u8 = 2;
const FREMOV: u8 = 4;

fn now() -> u32 {
    u32::try_from(daytime::now()).unwrap_or(0)
}

fn eprint(s: &str) {
    eprintln!("kfs: {s}");
}

#[derive(Clone)]
struct Fid {
    loc: Loc,
    path: u32,
    /// The directories walked through, the parent last.
    wpath: Vec<Loc>,
    open: u8,
    uid: i16,
    snap: Vec<u8>,
}

struct Kfs {
    fs: Fs<FileDev>,
    users: Users,
    writegroup: i16,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
    wstatallow: bool,
    writeallow: bool,
    noatime: bool,
    /// Exclusive-use files open now: qid path to fid.
    locks: BTreeMap<u32, u32>,
    /// Console output not yet read, per console fid.
    cons: BTreeMap<u32, Vec<u8>>,
}

fn qid(d: &Dentry) -> Qid {
    let mut qtype = QTFILE;
    if d.mode & DDIR != 0 {
        qtype |= QTDIR;
    }
    if d.mode & DAPND != 0 {
        qtype |= QTAPPEND;
    }
    if d.mode & DLOCK != 0 {
        qtype |= QTEXCL;
    }
    Qid {
        path: u64::from(d.path),
        vers: d.vers,
        qtype,
    }
}

fn err(s: &str) -> String {
    String::from(s)
}

/// Whether a name is one kfs can store.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.bytes().any(|b| b <= b' ' || b == b'/') {
        return Err(err(ENAME));
    }
    if name.len() + 1 > NAMELEN {
        return Err(err(ETOOLONG));
    }
    if name == "." || name == ".." {
        return Err(err(EDOT));
    }
    Ok(())
}

impl Kfs {
    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids.get(&fid).ok_or_else(|| err(EFID))
    }

    fn ro(&self, uid: i16) -> bool {
        self.fs.ro || (self.writegroup != NONE && !self.users.in_group(uid, self.writegroup))
    }

    /// Whether `uid` lacks permission `m` (`DREAD`, `DWRITE` or `DEXEC`)
    /// on `d`.
    fn denied(&self, d: &Dentry, m: u16, uid: i16) -> bool {
        if self.wstatallow {
            return false;
        }
        if uid != NONE {
            if uid == d.uid && (m << 6) & d.mode != 0 {
                return false;
            }
            if self.users.in_group(uid, d.gid) && (m << 3) & d.mode != 0 {
                return false;
            }
        }
        if m & d.mode != 0 {
            // Directories can always be walked, or noworld's could not be
            // reached.
            if d.mode & DDIR != 0 && m == DEXEC {
                return false;
            }
            if !self.users.in_group(uid, NOWORLD) {
                return false;
            }
        }
        true
    }

    fn getd(&mut self, f: &Fid) -> Result<Dentry, String> {
        self.fs.getd(f.loc, f.path)
    }

    /// Sets the access time of the entry at `loc`.
    fn touch(&mut self, loc: Loc, d: &mut Dentry) {
        if self.noatime || self.fs.ro {
            return;
        }
        d.atime = now();
        let _ = self.fs.putd(loc, d);
    }

    fn dir(&self, d: &Dentry) -> Dir {
        let mut mode = u32::from(d.mode & 0o777);
        if d.mode & DDIR != 0 {
            mode |= DMDIR;
        }
        if d.mode & DAPND != 0 {
            mode |= DMAPPEND;
        }
        if d.mode & DLOCK != 0 {
            mode |= DMEXCL;
        }
        Dir {
            dtype: 0,
            dev: 0,
            qid: qid(d),
            mode,
            atime: d.atime,
            mtime: d.mtime,
            length: if d.is_dir() { 0 } else { u64::from(d.size) },
            name: d.name.clone(),
            uid: self.users.name(d.uid),
            gid: self.users.name(d.gid),
            muid: self.users.name(d.muid),
        }
    }

    /// Reads `adm/users`. On failure the table is unchanged and the
    /// message is returned.
    fn read_users(&mut self) -> Result<(), String> {
        let text = match self.fs.read_path(&["adm", "users"]) {
            Ok(Some(t)) => t,
            Ok(None) => return Err(String::from("can't open /adm/users")),
            Err(e) => return Err(format!("/adm/users read error: {e}")),
        };
        self.users = Users::parse(&String::from_utf8_lossy(&text))?;
        self.writegroup = self.users.uid("write");
        Ok(())
    }

    fn walk1(&mut self, cur: &mut Fid, name: &str) -> Result<Qid, String> {
        let d = self.getd(cur)?;
        if !d.is_dir() {
            return Err(err(EDIR1));
        }
        if self.denied(&d, DEXEC, cur.uid) {
            return Err(err(EACCESS));
        }
        if name == "." || (name == ".." && cur.wpath.is_empty()) {
            return Ok(qid(&d));
        }
        if name == ".." {
            let up = cur.wpath.pop().expect("a parent");
            let pd = self.fs.getd(up, QPNONE)?;
            cur.loc = up;
            cur.path = pd.path;
            return Ok(qid(&pd));
        }
        let (loc, e) = self.fs.lookup(&d, name)?.ok_or_else(|| err(EENTRY))?;
        cur.wpath.push(cur.loc);
        cur.loc = loc;
        cur.path = e.path;
        Ok(qid(&e))
    }

    fn unlock(&mut self, fid: u32, f: &Fid) {
        if self.locks.get(&f.path) == Some(&fid) {
            self.locks.remove(&f.path);
        }
    }

    fn doremove(&mut self, f: &Fid) -> Result<(), String> {
        if self.ro(f.uid) {
            return Err(err(ERONLY));
        }
        let ploc = *f.wpath.last().ok_or_else(|| err(EPHASE))?;
        let mut pd = self.fs.getd(ploc, QPNONE)?;
        if self.denied(&pd, DWRITE, f.uid) {
            return Err(err(EACCESS));
        }
        self.getd(f)?;
        self.fs.remove(f.loc)?;
        let t = now();
        pd.mtime = t;
        pd.atime = t;
        pd.muid = f.uid;
        pd.vers = pd.vers.wrapping_add(1);
        self.fs.putd(ploc, &pd)
    }

    /// One console request: a command written, or its output read.
    fn console(&mut self, req: &Request) {
        if req.is_close() {
            self.cons.remove(&req.fid);
            return;
        }
        if req.is_read() {
            let out = self.cons.entry(req.fid).or_default();
            let n = out.len().min(req.count() as usize);
            let data: Vec<u8> = out.drain(..n).collect();
            let _ = req.respond_read(&data);
            return;
        }
        if req.is_trunc() {
            let _ = req.respond_write(0);
            return;
        }
        let text = String::from_utf8_lossy(req.data()).into_owned();
        let f: Vec<&str> = text.split_whitespace().collect();
        let mut out = Vec::new();
        let res = match f.first().copied() {
            None => Err(String::from("illegal kfs request")),
            Some("check") => match check_flags(f.get(1).copied().unwrap_or("")) {
                Some(flags) => {
                    out = kfs::check(&mut self.fs, flags);
                    Ok(())
                }
                None => Err(format!("illegal check flag: {}", f[1])),
            },
            Some("users" | "user") => {
                if let Err(e) = self.read_users() {
                    out.push(e);
                }
                Ok(())
            }
            Some("sync") => Ok(()),
            Some("allow") => {
                self.wstatallow = true;
                self.writeallow = true;
                Ok(())
            }
            Some("allowoff" | "disallow") => {
                self.wstatallow = false;
                self.writeallow = false;
                Ok(())
            }
            Some(_) => Err(String::from("unknown kfs request")),
        };
        match res {
            Ok(()) => {
                let buf = self.cons.entry(req.fid).or_default();
                buf.clear();
                for l in out {
                    buf.extend_from_slice(l.as_bytes());
                    buf.push(b'\n');
                }
                let _ = req.respond_write(req.count());
            }
            Err(e) => {
                let _ = req.respond_err(&e);
            }
        }
    }
}

impl StyxFs for Kfs {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, uname: &str, aname: &str) -> Result<Qid, String> {
        if !aname.is_empty() && aname != "main" {
            return Err(err(EBADSPC));
        }
        if self.fids.contains_key(&fid) {
            return Err(err(EFIDINUSE));
        }
        let mut d = self.fs.getd(ROOT, QPROOT).map_err(|_| err(EALLOC))?;
        if !d.is_dir() {
            return Err(err(EALLOC));
        }
        let uid = self.users.uid(uname);
        if self.denied(&d, DEXEC, uid) {
            return Err(err(EACCESS));
        }
        self.touch(ROOT, &mut d);
        self.fids.insert(
            fid,
            Fid {
                loc: ROOT,
                path: QPROOT,
                wpath: Vec::new(),
                open: 0,
                uid,
                snap: Vec::new(),
            },
        );
        Ok(qid(&d))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        let f = self.fid(fid)?.clone();
        // Inferno's kfs refuses to walk any open fid. The kernel's mount
        // driver here opens a mount's root fid itself (`ls` of the mount
        // point) and later clones it, so only walking an open fid in
        // place is refused.
        if f.open != 0 && newfid == fid {
            return Err(err(EMODE));
        }
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(err(EFIDINUSE));
        }
        let mut cur = f;
        cur.open = 0;
        let mut qids = Vec::new();
        for name in names {
            match self.walk1(&mut cur, name) {
                Ok(q) => qids.push(q),
                Err(e) if qids.is_empty() => return Err(e),
                Err(_) => break,
            }
        }
        if qids.len() == names.len() {
            cur.snap = Vec::new();
            self.fids.insert(newfid, cur);
        }
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let f = self.fid(fid)?.clone();
        let wok = self.writeallow;
        let ro = self.ro(f.uid);
        if omode & mode::ORCLOSE != 0 {
            if ro {
                return Err(err(ERONLY));
            }
            let ploc = *f.wpath.last().ok_or_else(|| err(EPHASE))?;
            let pd = self.fs.getd(ploc, QPNONE).map_err(|_| err(EPHASE))?;
            if self.denied(&pd, DWRITE, f.uid) {
                return Err(err(EACCESS));
            }
        }
        let mut d = self.getd(&f)?;
        let dir = d.is_dir();
        let deny = |k: &Kfs, m| k.denied(&d, m, f.uid) && !wok;
        let mut fmod = match omode & mode::ACCESS {
            mode::OREAD => {
                if deny(self, DREAD) {
                    return Err(err(EACCESS));
                }
                FREAD
            }
            mode::OWRITE => {
                if dir || deny(self, DWRITE) {
                    return Err(err(EACCESS));
                }
                if ro {
                    return Err(err(ERONLY));
                }
                FWRITE
            }
            mode::ORDWR => {
                if dir || deny(self, DREAD) || deny(self, DWRITE) {
                    return Err(err(EACCESS));
                }
                if ro {
                    return Err(err(ERONLY));
                }
                FREAD | FWRITE
            }
            _ => {
                if dir || deny(self, DEXEC) {
                    return Err(err(EACCESS));
                }
                FREAD
            }
        };
        if omode & mode::OTRUNC != 0 {
            if dir || deny(self, DWRITE) {
                return Err(err(EACCESS));
            }
            if ro {
                return Err(err(ERONLY));
            }
        }
        if d.mode & DLOCK != 0 {
            if self.locks.get(&d.path).is_some_and(|&o| o != fid) {
                return Err(err(ELOCKED));
            }
            self.locks.insert(d.path, fid);
        }
        if omode & mode::ORCLOSE != 0 {
            fmod |= FREMOV;
        }
        if omode & mode::OTRUNC != 0 && d.mode & DAPND == 0 {
            self.fs.trunc(&mut d)?;
            let t = now();
            d.mtime = t;
            d.atime = t;
            d.muid = f.uid;
            d.vers = d.vers.wrapping_add(1);
            self.fs.putd(f.loc, &d)?;
        }
        if let Some(x) = self.fids.get_mut(&fid) {
            x.open = fmod;
            x.snap = Vec::new();
        }
        Ok((qid(&d), self.iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, omode: u8) -> Result<(Qid, u32), String> {
        let f = self.fid(fid)?.clone();
        if self.ro(f.uid) {
            return Err(err(ERONLY));
        }
        let d = self.getd(&f)?;
        if !d.is_dir() {
            return Err(err(EDIR2));
        }
        if self.denied(&d, DWRITE, f.uid) && !self.writeallow {
            return Err(err(EACCESS));
        }
        check_name(name)?;
        let mut fmod = match omode & mode::ACCESS {
            mode::OWRITE => FWRITE,
            mode::ORDWR => FREAD | FWRITE,
            _ => FREAD,
        };
        if perm & DMDIR != 0
            && (omode & mode::OTRUNC != 0 || perm & DMAPPEND != 0 || fmod & FWRITE != 0)
        {
            return Err(err(EACCESS));
        }
        let mut perm = perm & (u32::from(d.mode) | !0o666);
        if perm & DMDIR != 0 {
            perm &= u32::from(d.mode) | !0o777;
        }
        let mut m = (perm & 0o777) as u16;
        if perm & DMDIR != 0 {
            m |= DDIR;
        }
        if perm & DMAPPEND != 0 {
            m |= DAPND;
        }
        if perm & DMEXCL != 0 {
            m |= DLOCK;
        }
        let t = now();
        let e = Dentry {
            name: String::from(name),
            uid: f.uid,
            gid: d.gid,
            muid: f.uid,
            mode: m,
            atime: t,
            mtime: t,
            ..Dentry::default()
        };
        let (loc, nd) = self.fs.create(f.loc, &e)?;
        // The directory may have grown a block: read it again.
        let mut pd = self.getd(&f)?;
        pd.mtime = t;
        pd.atime = t;
        pd.muid = f.uid;
        pd.vers = pd.vers.wrapping_add(1);
        self.fs.putd(f.loc, &pd)?;
        if m & DLOCK != 0 {
            self.locks.insert(nd.path, fid);
        }
        if omode & mode::ORCLOSE != 0 {
            fmod |= FREMOV;
        }
        if let Some(x) = self.fids.get_mut(&fid) {
            x.wpath.push(x.loc);
            x.loc = loc;
            x.path = nd.path;
            x.open = fmod;
            x.snap = Vec::new();
        }
        Ok((qid(&nd), self.iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?.clone();
        if f.open & FREAD == 0 {
            return Err(err(EOPEN));
        }
        let count = count.min(self.iounit);
        let mut d = self.getd(&f)?;
        self.touch(f.loc, &mut d);
        if d.is_dir() {
            if offset == 0 || f.snap.is_empty() {
                let mut snap = Vec::new();
                for (_, e) in self.fs.entries(&d)? {
                    self.dir(&e)
                        .pack_into(&mut snap)
                        .map_err(|e| e.to_string())?;
                }
                if let Some(x) = self.fids.get_mut(&fid) {
                    x.snap = snap;
                }
            }
            return Ok(styxserv::rotree::dir_slice(
                &self.fid(fid)?.snap,
                offset,
                count,
            ));
        }
        self.fs.read(&d, offset, count as usize)
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let f = self.fid(fid)?.clone();
        if f.open & FWRITE == 0 {
            return Err(err(EOPEN));
        }
        if self.ro(f.uid) {
            return Err(err(ERONLY));
        }
        if data.len() > self.iounit as usize {
            return Err(err(ECOUNT));
        }
        let n = self.fs.write(f.loc, f.path, offset, data, f.uid, now())?;
        Ok(n as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self.fids.remove(&fid).ok_or_else(|| err(EFID))?;
        self.unlock(fid, &f);
        if f.open & FREMOV != 0 {
            let _ = self.doremove(&f);
        }
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let f = self.fids.remove(&fid).ok_or_else(|| err(EFID))?;
        self.unlock(fid, &f);
        self.doremove(&f)
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?.clone();
        let d = self.getd(&f)?;
        let mut dir = self.dir(&d);
        if d.path == QPROOT {
            dir.atime = now();
        }
        dir.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        let f = self.fid(fid)?.clone();
        if f.uid == NONE && !self.wstatallow {
            return Err(err(EACCESS));
        }
        if self.ro(f.uid) {
            return Err(err(ERONLY));
        }
        let parent = match f.wpath.last() {
            Some(&ploc) => Some(self.fs.getd(ploc, QPNONE).map_err(|_| err(EPHASE))?),
            None => None,
        };
        let mut d = self.getd(&f)?;
        let (dir, _) = Dir::unpack(stat).map_err(|e| e.to_string())?;
        let uid = if dir.uid.is_empty() {
            d.uid
        } else {
            self.users.uid(&dir.uid)
        };
        let gid = if dir.gid.is_empty() {
            d.gid
        } else {
            self.users.uid(&dir.gid)
        };
        let name = if dir.name.is_empty() {
            d.name.clone()
        } else {
            if dir.name.bytes().any(|b| b <= b' ' || b == b'/') {
                return Err(err(ENAME));
            }
            if dir.name.len() + 1 > NAMELEN {
                return Err(err(ETOOLONG));
            }
            dir.name.clone()
        };
        let qtype = dir.qid.qtype;
        let mut dmode = dir.mode;
        if qtype == 0xff && dmode == !0 {
            dmode = u32::from(d.mode & 0o777);
            if d.mode & DLOCK != 0 {
                dmode |= DMEXCL;
            }
            if d.mode & DAPND != 0 {
                dmode |= DMAPPEND;
            }
            if d.mode & DDIR != 0 {
                dmode |= DMDIR;
            }
        } else if qtype == 0xff {
        } else if dmode == !0 {
            dmode = (u32::from(qtype) << 24) | u32::from(d.mode & 0o777);
        } else if u32::from(qtype) != (dmode >> 24) & 0xff {
            return Err(err(EQIDMODE));
        }
        if dmode & !(DMDIR | DMAPPEND | DMEXCL | 0o777) != 0 {
            return Err(err(ENOTM));
        }
        let isdir = if d.is_dir() { DMDIR } else { 0 };
        if (dmode ^ isdir) & DMDIR != 0 {
            return Err(err(ENOTD));
        }
        let mtime = if dir.mtime == !0 { d.mtime } else { dir.mtime };
        let length = if dir.length == !0 {
            u64::from(d.size)
        } else {
            dir.length
        };
        if length != u64::from(d.size) && !d.is_dir() {
            return Err(err(ENOTL));
        }
        if uid != d.uid && !self.wstatallow {
            return Err(err(ENOTU));
        }
        if gid != d.gid
            && !(self.wstatallow
                || self.writeallow
                || (d.uid == f.uid && self.users.in_group(f.uid, gid))
                || (self.users.leads(f.uid, gid) && self.users.leads(f.uid, d.gid)))
        {
            return Err(err(ENOTG));
        }
        if name != d.name {
            let Some(pd) = &parent else {
                return Err(err(ENAME));
            };
            if name == "." || name == ".." {
                return Err(err(EDOT));
            }
            if self.fs.lookup(pd, &name)?.is_some() {
                return Err(err(EEXIST));
            }
            if !(self.wstatallow || self.writeallow) && self.denied(pd, DWRITE, f.uid) {
                return Err(err(EACCESS));
            }
        }
        let mut m = (dmode & 0o777) as u16;
        if dmode & DMAPPEND != 0 {
            m |= DAPND;
        }
        if dmode & DMEXCL != 0 {
            m |= DLOCK;
        }
        if (mtime != d.mtime || (d.mode ^ m) & (DAPND | DLOCK | 0o777) != 0)
            && !(self.wstatallow
                || d.uid == f.uid
                || self.users.leads(f.uid, gid)
                || self.users.leads(f.uid, d.gid))
        {
            return Err(err(ENOTU));
        }
        d.mtime = mtime;
        d.uid = uid;
        d.gid = gid;
        d.mode = m | (d.mode & (DALLOC | DDIR));
        d.name = name;
        if !self.noatime {
            d.atime = now();
        }
        self.fs.putd(f.loc, &d)
    }
}

/// Speaks 9P on fd 0, and serves the console when there is one.
fn serve(k: &mut Kfs, mut console: Option<FileIO>) -> Result<(), Error> {
    // SAFETY: fd 0 is the conversation `mount` gave this prog; nothing
    // else in the prog uses it.
    let mut io = unsafe { File::from_raw_fd(0) };
    let mut msize = MAXRPC;
    loop {
        if let Some(c) = &console {
            match file2chan::recv_any_or_fds(&[c], &[0])? {
                Event::Fd(_) => {}
                Event::File(_, Some(req)) => {
                    k.console(&req);
                    continue;
                }
                Event::File(_, None) => {
                    console = None;
                    continue;
                }
            }
        }
        let buf = match styxserv::read_msg(&mut io) {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(()),
            Err(e) => return Err(Error(e.to_string())),
        };
        let msg = styxserv::decode(&buf).map_err(Error)?;
        let reply = purg_styx::dispatch(k, &msg, &mut msize);
        io.write_all(&reply)?;
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut bsize = 1024usize;
    let (mut ream, mut nocheck, mut ro) = (false, false, false);
    let (mut wstatallow, mut writeallow, mut noatime) = (false, false, false);
    let mut name = None;
    while let Some(c) = arg.opt() {
        match c {
            'r' => ream = true,
            'b' => {
                let s = arg.earg()?;
                bsize = s.parse().unwrap_or(0);
            }
            'c' => nocheck = true,
            'A' => noatime = true,
            'D' => {}
            'P' => writeallow = true,
            'R' => ro = true,
            'W' => wstatallow = true,
            'n' => name = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let [path] = arg.argv().try_into().map_err(|_| usage)?;
    let file = OpenOptions::new()
        .read(true)
        .write(!ro)
        .open(&path)
        .map_err(|e| Error(format!("can't open {path}: {}", Error::from(e))))?;
    let fs = if ream {
        if ro {
            return Err(Error::from("cannot ream a read-only file system"));
        }
        if bsize == 0 || !bsize.is_multiple_of(512) || bsize > MAXBUFSIZE {
            return Err(Error(format!("invalid block size {bsize}")));
        }
        eprint(&format!(
            "reaming the file system using {bsize} byte blocks"
        ));
        Fs::ream(FileDev(file), bsize, now()).map_err(Error)?
    } else {
        Fs::open(FileDev(file), ro).map_err(Error)?
    };
    let mut k = Kfs {
        fs,
        users: Users::default(),
        writegroup: NONE,
        fids: BTreeMap::new(),
        iounit: MAXRPC - IOHDRSZ,
        wstatallow: wstatallow || ream,
        writeallow: writeallow || ream,
        noatime,
        locks: BTreeMap::new(),
        cons: BTreeMap::new(),
    };
    if !ream && !ro && !k.fs.set_fsok(false).map_err(Error)? {
        eprintln!("kfs needs check");
        if !nocheck {
            let f = check_flags("qf").expect("good flags");
            for l in kfs::check(&mut k.fs, f) {
                eprint(&l);
            }
        }
    }
    if k.read_users().is_err() {
        eprint("initializing minimal user table");
        k.users = Users::default();
        k.writegroup = k.users.uid("write");
    }
    let console = match &name {
        Some(n) => Some(
            file2chan::file2chan("/chan", &format!("kfs.{n}.cmd"))
                .map_err(|e| Error(format!("can't create /chan/kfs.{n}.cmd: {e}")))?,
        ),
        None => None,
    };
    let res = serve(&mut k, console);
    if !k.fs.ro {
        k.fs.set_fsok(true).map_err(Error)?;
    }
    res
}
