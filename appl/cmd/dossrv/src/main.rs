//! dossrv [-s] [-vFc] [-f devicefile] [-m mountpoint] [-S secs]: serve
//! the FAT file system in `devicefile` (default `/dev/hd0disk`), read and
//! write, on `mountpoint` (default `/n/dos`), or with `-s` on standard
//! output. FAT12, FAT16 and FAT32 are served, with long names: a name
//! that is not a lower-case 8.3 name is stored as a long name with a
//! made-up short one (`NAME~1.EXT`), and look-ups ignore case. A volume
//! may start inside the device: an attach name `device:n` serves the
//! volume at sector `n` of `device`, and a device that starts with a
//! partition table serves its first FAT partition.
//!
//! Files belong to `dos`, group `srv`. A file has mode 0666, or 0444 when
//! it is read-only; a directory adds 0111, and the root is 0777. The
//! system attribute is `DMEXCL`. wstat can rename an entry in its
//! directory, set the time, clear or set read-only (from the owner's
//! write bit) and system (from `DMEXCL`), and truncate a file to 0.
//!
//! Follows Inferno's dossrv(4). Where it differs: FAT32 is served; a
//! mode need not have the same bits for owner, group and others; `-v`,
//! `-F`, `-c` and `-S` are accepted and do nothing. Times are UTC.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};

use fat::{ATTR_DIR, ATTR_RO, ATTR_SYSTEM, DirRef, Ent, Image, Loc, Vol};
use purg_styx::{IOHDRSZ, QTDIR, QTEXCL, QTFILE, Qid};
use purgatorio::abi::mount::{MCREATE, MREPL};
use purgatorio::abi::{error, mode};
use purgatorio::arg::Arg;
use purgatorio::styx::StyxFs;
use purgatorio::{Context, DMDIR, DMEXCL, Dir, Error, daytime};

const USAGE: &str = "dossrv [-s] [-vFc] [-f devicefile] [-m mountpoint] [-S secs]";

type V = Vol<Image<File>>;

/// Opens the volume `spec` names: `device` or `device:sector`.
fn open_vol(spec: &str) -> Result<V, String> {
    let (dev, at) = match spec.rsplit_once(':') {
        Some((d, n)) if !d.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty() => {
            (d, n.parse::<u64>().map_err(|_| format!("bad sector {n}"))?)
        }
        _ => (spec, 0),
    };
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev)
            .map_err(|e| format!("cannot open {dev}: {}", styxserv::ioerr(e)))
    };
    match Vol::open(Image(open()?), at * 512) {
        Ok(v) => Ok(v),
        Err(e) if at != 0 => Err(format!("{spec}: {e}")),
        Err(e) => {
            // A partition table: the first FAT partition.
            let mut mbr = [0u8; 512];
            let mut img = Image(open()?);
            if fat::Disk::read_at(&mut img, 0, &mut mbr).is_err() {
                return Err(format!("{spec}: {e}"));
            }
            for p in 0..4 {
                let entry = &mbr[446 + 16 * p..462 + 16 * p];
                if matches!(entry[4], 0x01 | 0x04 | 0x06 | 0x0b | 0x0c | 0x0e) {
                    let lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
                    if let Ok(v) = Vol::open(img, u64::from(lba) * 512) {
                        return Ok(v);
                    }
                    break;
                }
            }
            Err(format!("{spec}: {e}"))
        }
    }
}

struct Fid {
    vol: usize,
    /// The entries from the root down to this file; empty for the root.
    path: Vec<Loc>,
    open: Option<u8>,
    /// A directory's listing, made at offset 0.
    snap: Vec<u8>,
}

struct Dos {
    device: String,
    vols: Vec<(String, V)>,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

fn now() -> u32 {
    u32::try_from(daytime::now()).unwrap_or(0)
}

fn qid_of(e: Option<&Ent>) -> Qid {
    match e {
        None => Qid {
            path: 0,
            vers: 0,
            qtype: QTDIR,
        },
        Some(e) => {
            // The entry's slot names it.
            let dir = match e.loc.dir {
                DirRef::Root => 0,
                DirRef::Chain(c) => u64::from(c),
            };
            let qtype = if e.is_dir() {
                QTDIR
            } else if e.attr & ATTR_SYSTEM != 0 {
                QTEXCL
            } else {
                QTFILE
            };
            Qid {
                path: (dir << 32 | u64::from(e.loc.slot)) + 1,
                vers: 0,
                qtype,
            }
        }
    }
}

fn dir_of(e: Option<&Ent>) -> Dir {
    let (name, mode, length, mtime) = match e {
        None => (String::from("/"), DMDIR | 0o777, 0, 0),
        Some(e) => {
            let mut m = if e.readonly() { 0o444 } else { 0o666 };
            if e.is_dir() {
                m |= DMDIR | 0o111;
            }
            if e.attr & ATTR_SYSTEM != 0 {
                m |= DMEXCL;
            }
            let len = if e.is_dir() { 0 } else { u64::from(e.size) };
            (e.name.clone(), m, len, e.mtime)
        }
    };
    Dir {
        qid: qid_of(e),
        mode,
        atime: mtime,
        mtime,
        length,
        name,
        uid: String::from("dos"),
        gid: String::from("srv"),
        muid: String::from("dos"),
        ..Dir::default()
    }
}

fn eperm() -> String {
    String::from(error::EPERM)
}

impl Dos {
    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn vol(&mut self, n: usize) -> &mut V {
        &mut self.vols[n].1
    }

    /// The fid's entry as it is now; `None` for the root.
    fn ent(&mut self, fid: u32) -> Result<(usize, Option<Ent>), String> {
        let f = self.fid(fid)?;
        let (v, last) = (f.vol, f.path.last().copied());
        match last {
            None => Ok((v, None)),
            Some(loc) => Ok((v, Some(self.vol(v).get_ent(loc)?))),
        }
    }

    fn dirref(&mut self, v: usize, e: Option<&Ent>) -> Result<DirRef, String> {
        match e {
            None => Ok(self.vol(v).root()),
            Some(e) if e.is_dir() => Ok(e.as_dir()),
            Some(_) => Err(String::from(error::ENOTDIR)),
        }
    }

    fn listing(&mut self, v: usize, d: DirRef) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for e in self.vol(v).list(d)? {
            let _ = dir_of(Some(&e)).pack_into(&mut out);
        }
        Ok(out)
    }

    /// Whether the directory holding the fid's entry is read-only.
    fn parent_ro(&mut self, fid: u32) -> Result<bool, String> {
        let f = self.fid(fid)?;
        let (v, path) = (f.vol, f.path.clone());
        if path.len() < 2 {
            return Ok(false);
        }
        Ok(self.vol(v).get_ent(path[path.len() - 2])?.readonly())
    }

    /// Points every fid at `old` to `new` after a rename.
    fn moved(&mut self, v: usize, old: Loc, new: Loc) {
        for f in self.fids.values_mut() {
            if f.vol == v {
                for l in f.path.iter_mut() {
                    if *l == old {
                        *l = new;
                    }
                }
            }
        }
    }
}

impl StyxFs for Dos {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        let spec = if aname.is_empty() {
            self.device.clone()
        } else {
            String::from(aname)
        };
        let vol = match self.vols.iter().position(|(s, _)| *s == spec) {
            Some(v) => v,
            None => {
                let v = open_vol(&spec)?;
                self.vols.push((spec, v));
                self.vols.len() - 1
            }
        };
        self.fids.insert(
            fid,
            Fid {
                vol,
                path: Vec::new(),
                open: None,
                snap: Vec::new(),
            },
        );
        Ok(qid_of(None))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let f = self.fid(fid)?;
        let (v, mut path) = (f.vol, f.path.clone());
        let mut qids = Vec::new();
        for name in names {
            let cur = match path.last() {
                None => None,
                Some(&l) => Some(self.vol(v).get_ent(l)?),
            };
            let step = match self.dirref(v, cur.as_ref()) {
                Err(e) => Err(e),
                Ok(_) if *name == ".." => {
                    path.pop();
                    let up = match path.last() {
                        None => None,
                        Some(&l) => Some(self.vol(v).get_ent(l)?),
                    };
                    Ok(qid_of(up.as_ref()))
                }
                Ok(d) => match self.vol(v).lookup(d, name)? {
                    Some(e) => {
                        path.push(e.loc);
                        Ok(qid_of(Some(&e)))
                    }
                    None => Err(String::from(error::ENONEXIST)),
                },
            };
            match step {
                Ok(q) => qids.push(q),
                Err(e) if qids.is_empty() => return Err(e),
                Err(_) => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                vol: v,
                path,
                open: None,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        if self.fid(fid)?.open.is_some() {
            return Err(String::from("fid already open"));
        }
        let (v, e) = self.ent(fid)?;
        let access = omode & mode::ACCESS;
        let writes = matches!(access, mode::OWRITE | mode::ORDWR) || omode & mode::OTRUNC != 0;
        let isdir = e.as_ref().is_none_or(Ent::is_dir);
        if writes && (isdir || e.as_ref().is_some_and(Ent::readonly)) {
            return Err(eperm());
        }
        if omode & mode::ORCLOSE != 0 && (e.is_none() || self.parent_ro(fid)?) {
            return Err(eperm());
        }
        if let Some(e) = &e
            && omode & mode::OTRUNC != 0
        {
            self.vol(v).truncate(e.loc, now())?;
        }
        if let Some(f) = self.fids.get_mut(&fid) {
            f.open = Some(omode);
        }
        Ok((qid_of(e.as_ref()), self.iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, omode: u8) -> Result<(Qid, u32), String> {
        if self.fid(fid)?.open.is_some() {
            return Err(String::from("create on open file"));
        }
        let (v, e) = self.ent(fid)?;
        if e.as_ref().is_some_and(|e| !e.is_dir()) {
            return Err(String::from("create in non-directory"));
        }
        if e.as_ref().is_some_and(Ent::readonly) {
            return Err(eperm());
        }
        if name == "." || name == ".." {
            return Err(String::from(fat::ESYNTAX));
        }
        let d = self.dirref(v, e.as_ref())?;
        let mut attr = 0;
        if perm & DMDIR != 0 {
            attr |= ATTR_DIR;
            if omode & mode::ACCESS != mode::OREAD {
                return Err(eperm());
            }
        }
        if perm & 0o222 == 0 {
            attr |= ATTR_RO;
        }
        if perm & DMEXCL != 0 {
            attr |= ATTR_SYSTEM;
        }
        let made = self.vol(v).create(d, name, attr, now())?;
        if let Some(f) = self.fids.get_mut(&fid) {
            f.path.push(made.loc);
            f.open = Some(omode);
        }
        Ok((qid_of(Some(&made)), self.iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        if self.fid(fid)?.open.is_none() {
            return Err(String::from("fid not open"));
        }
        let (v, e) = self.ent(fid)?;
        if e.as_ref().is_none_or(Ent::is_dir) {
            if offset == 0 {
                let d = self.dirref(v, e.as_ref())?;
                let snap = self.listing(v, d)?;
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.snap = snap;
                }
            }
            return Ok(styxserv::rotree::dir_slice(
                &self.fid(fid)?.snap,
                offset,
                count,
            ));
        }
        let e = e.expect("a file");
        self.vol(v).read(&e, offset, count as usize)
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let f = self.fid(fid)?;
        if !f
            .open
            .is_some_and(|m| matches!(m & mode::ACCESS, mode::OWRITE | mode::ORDWR))
        {
            return Err(String::from("fid not open for write"));
        }
        let (v, e) = self.ent(fid)?;
        let e = e.ok_or_else(eperm)?;
        if e.is_dir() {
            return Err(eperm());
        }
        let n = self.vol(v).write(e.loc, offset, data, now())?;
        Ok(n as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let v = f.vol;
        if f.open.is_some_and(|m| m & mode::ORCLOSE != 0)
            && let Some(&loc) = f.path.last()
            && let Ok(e) = self.vol(v).get_ent(loc)
        {
            let _ = self.vol(v).remove(&e);
        }
        self.vol(v).flush()
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let res = (|| {
            let (v, e) = self.ent(fid)?;
            let e = e.ok_or_else(eperm)?;
            if e.readonly() || self.parent_ro(fid)? {
                return Err(eperm());
            }
            self.vol(v).remove(&e)
        })();
        self.fids.remove(&fid);
        res
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let (_, e) = self.ent(fid)?;
        dir_of(e.as_ref()).pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        let (d, _) = Dir::unpack(stat).map_err(|e| e.to_string())?;
        let (v, e) = self.ent(fid)?;
        let Some(mut e) = e else {
            return Err(eperm());
        };
        let cur = dir_of(Some(&e));
        let changes = |new: &str, old: &str| !new.is_empty() && new != old;
        if changes(&d.uid, &cur.uid) || changes(&d.gid, &cur.gid) {
            return Err(eperm());
        }
        if d.mode != !0 && (d.mode & DMDIR) != (cur.mode & DMDIR) {
            return Err(eperm());
        }
        if d.length != !0 && d.length != cur.length && (d.length != 0 || e.is_dir()) {
            return Err(eperm());
        }
        if changes(&d.name, &cur.name) {
            if e.readonly() || self.parent_ro(fid)? {
                return Err(eperm());
            }
            if d.name.contains('/') || d.name == "." || d.name == ".." {
                return Err(String::from(fat::ESYNTAX));
            }
            let old = e.loc;
            e = self.vol(v).rename(&e, &d.name)?;
            self.moved(v, old, e.loc);
        }
        if d.length == 0 && cur.length != 0 && !e.is_dir() {
            self.vol(v).truncate(e.loc, now())?;
        }
        if d.mode != !0 {
            let mut attr = 0;
            if d.mode & 0o200 == 0 {
                attr |= ATTR_RO;
            }
            if d.mode & DMEXCL != 0 {
                attr |= ATTR_SYSTEM;
            }
            self.vol(v).set_attr(e.loc, attr, ATTR_RO | ATTR_SYSTEM)?;
        }
        if d.mtime != !0 {
            self.vol(v).set_mtime(e.loc, d.mtime)?;
        }
        self.vol(v).flush()
    }
}

fn server(device: &str) -> Dos {
    Dos {
        device: String::from(device),
        vols: Vec::new(),
        fids: BTreeMap::new(),
        iounit: 8192,
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let dev = args.get(2).cloned().unwrap_or_default();
        return styxserv::bg::serve(&mut server(&dev));
    }
    let mut arg = Arg::new(args, USAGE);
    let mut device = String::from("/dev/hd0disk");
    let mut mtpt = String::from("/n/dos");
    let mut stdout = false;
    while let Some(c) = arg.opt() {
        match c {
            'f' => device = arg.earg()?,
            'm' => mtpt = arg.earg()?,
            'S' => {
                arg.earg()?;
            }
            's' => stdout = true,
            'v' | 'F' | 'c' => {}
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let device = styxserv::abs(&device);
    // Open it here too, so a bad device is this command's error.
    open_vol(&device)?;
    if stdout {
        // SAFETY: fd 1 is the conversation, and nothing else uses it.
        let io = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(1) };
        return purgatorio::styx::serve(io, &mut server(&device));
    }
    let argv = [String::from("dossrv"), String::from("--serve"), device];
    styxserv::bg::mount("/bin/dossrv.wasm", &argv, &mtpt, MREPL | MCREATE)
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")))
}
