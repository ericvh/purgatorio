//! ndb/registry [-f dbfile]: serve, on fd 0, a registry of services, each
//! a name with `attr value` pairs, as `mount {ndb/registry} /mnt/registry`.
//!
//! - `new`: the first write, `name [attr value ...]`, registers a service
//!   owned by the writer. It goes away when that file is closed, unless it
//!   has a `persist` attribute other than `0`.
//! - `index`: one line per service, its name and then its pairs.
//! - `find`: a write of `attr value ...` (a value of `*` matches anything;
//!   the attribute `name` matches the service's name) sets this open
//!   file's filter; reads from offset 0 return the matching services, as
//!   `index` does.
//! - one file per service, holding its pairs. Its owner may write pairs to
//!   add or change attributes, and remove it to take the service away.
//!
//! Text is words separated by blanks, quoted with `'` as in attrdb. With
//! `-f`, each entry of `dbfile` with a `service=name` pair is a service
//! that always exists. The `event` file is not served yet: a server here
//! answers each request before the next, so it cannot hold a read open.

use std::collections::BTreeMap;
use std::os::fd::FromRawFd;

use ndb::quote;
use purgatorio::abi::{error, mode};
use purgatorio::arg::Arg;
use purgatorio::styx::{self, IOHDRSZ, StyxFs};
use purgatorio::{Context, DMDIR, Dir, Error, Qid};

const QTDIR: u8 = 0x80;
const QTFILE: u8 = 0;

const ROOT: u64 = 0;
const NEW: u64 = 1;
const INDEX: u64 = 2;
const FIND: u64 = 3;
/// Service files have paths from here on.
const SERVICE: u64 = 16;

struct Service {
    name: String,
    attrs: Vec<(String, String)>,
    owner: String,
    /// Made by this `new` fid, which removes it on clunk.
    by: Option<u32>,
}

#[derive(Clone)]
struct Fid {
    path: u64,
    uname: String,
    open: bool,
    filter: Vec<(String, String)>,
    snap: Vec<u8>,
}

struct Registry {
    services: BTreeMap<u64, Service>,
    fids: BTreeMap<u32, Fid>,
    next: u64,
    vers: u32,
    iounit: u32,
}

impl Registry {
    fn qid(&self, path: u64) -> Qid {
        Qid {
            path,
            vers: if path == ROOT { self.vers } else { 0 },
            qtype: if path == ROOT { QTDIR } else { QTFILE },
        }
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn by_name(&self, name: &str) -> Option<u64> {
        self.services
            .iter()
            .find(|(_, s)| s.name == name)
            .map(|(p, _)| *p)
    }

    fn dir(&self, path: u64) -> Option<Dir> {
        let (name, mode, owner) = match path {
            ROOT => (String::from("."), DMDIR | 0o555, String::from("registry")),
            NEW => (String::from("new"), 0o666, String::from("registry")),
            INDEX => (String::from("index"), 0o444, String::from("registry")),
            FIND => (String::from("find"), 0o666, String::from("registry")),
            p => {
                let s = self.services.get(&p)?;
                (s.name.clone(), 0o644, s.owner.clone())
            }
        };
        Some(Dir {
            qid: self.qid(path),
            mode,
            name,
            uid: owner.clone(),
            gid: owner.clone(),
            muid: owner,
            ..Dir::default()
        })
    }

    fn line(s: &Service) -> String {
        let mut w = vec![quote(&s.name)];
        for (a, v) in &s.attrs {
            w.push(quote(a));
            w.push(quote(v));
        }
        w.join(" ") + "\n"
    }

    fn matches(s: &Service, filter: &[(String, String)]) -> bool {
        filter.iter().all(|(a, v)| {
            if a == "name" {
                return v == "*" || *v == s.name;
            }
            s.attrs
                .iter()
                .any(|(sa, sv)| sa == a && (v == "*" || sv == v))
        })
    }

    fn text(&self, path: u64, filter: Option<&[(String, String)]>) -> Vec<u8> {
        match path {
            INDEX | FIND => {
                let mut out = String::new();
                for s in self.services.values() {
                    if filter.is_none_or(|f| Registry::matches(s, f)) {
                        out.push_str(&Registry::line(s));
                    }
                }
                out.into_bytes()
            }
            p => match self.services.get(&p) {
                Some(s) => {
                    let mut w = Vec::new();
                    for (a, v) in &s.attrs {
                        w.push(quote(a));
                        w.push(quote(v));
                    }
                    (w.join(" ") + "\n").into_bytes()
                }
                None => Vec::new(),
            },
        }
    }

    fn listing(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let paths = [NEW, INDEX, FIND]
            .into_iter()
            .chain(self.services.keys().copied());
        for p in paths {
            if let Some(d) = self.dir(p) {
                let _ = d.pack_into(&mut out);
            }
        }
        out
    }

    fn changed(&mut self) {
        self.vers = self.vers.wrapping_add(1);
    }

    /// Adds or changes `attr value` pairs.
    fn set(attrs: &mut Vec<(String, String)>, pairs: Vec<(String, String)>) {
        for (a, v) in pairs {
            match attrs.iter_mut().find(|(x, _)| *x == a) {
                Some(slot) => slot.1 = v,
                None => attrs.push((a, v)),
            }
        }
    }
}

/// Words as `attr value` pairs.
fn pairs(words: &[String]) -> Result<Vec<(String, String)>, String> {
    if !words.len().is_multiple_of(2) {
        return Err(String::from("odd number of attributes and values"));
    }
    Ok(words
        .chunks(2)
        .map(|p| (p[0].clone(), p[1].clone()))
        .collect())
}

fn page(snap: &[u8], offset: u64, count: u32) -> Vec<u8> {
    let start = (offset as usize).min(snap.len());
    let end = (start + count as usize).min(snap.len());
    snap[start..end].to_vec()
}

impl StyxFs for Registry {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                path: ROOT,
                uname: String::from(uname),
                open: false,
                filter: Vec::new(),
                snap: Vec::new(),
            },
        );
        Ok(self.qid(ROOT))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let f = self.fid(fid)?.clone();
        let mut at = f.path;
        let mut qids = Vec::new();
        for name in names {
            let next = match (at, *name) {
                (ROOT, "..") => Some(ROOT),
                (ROOT, "new") => Some(NEW),
                (ROOT, "index") => Some(INDEX),
                (ROOT, "find") => Some(FIND),
                (ROOT, n) => self.by_name(n),
                _ => None,
            };
            let Some(next) = next else {
                if qids.is_empty() {
                    return Err(String::from(error::ENONEXIST));
                }
                return Ok(qids);
            };
            at = next;
            qids.push(self.qid(at));
        }
        self.fids.insert(
            newfid,
            Fid {
                path: at,
                uname: f.uname,
                open: false,
                filter: Vec::new(),
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let iounit = self.iounit;
        let path = self.fid(fid)?.path;
        let write = omode & 3 != mode::OREAD;
        let ok = match path {
            ROOT | INDEX => !write,
            NEW | FIND => true,
            p => {
                let s = self.services.get(&p).ok_or(error::ENONEXIST)?;
                !write || s.owner == self.fid(fid)?.uname
            }
        };
        if !ok {
            return Err(String::from(error::EPERM));
        }
        let qid = self.qid(path);
        if let Some(f) = self.fids.get_mut(&fid) {
            f.open = true;
        }
        Ok((qid, iounit))
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
        let f = self.fid(fid)?.clone();
        if offset == 0 || f.snap.is_empty() {
            let snap = match f.path {
                ROOT => self.listing(),
                NEW => Vec::new(),
                FIND => self.text(FIND, Some(&f.filter)),
                p => self.text(p, None),
            };
            if let Some(f) = self.fids.get_mut(&fid) {
                f.snap = snap;
            }
        }
        let snap = &self.fid(fid)?.snap;
        if f.path != ROOT {
            return Ok(page(snap, offset, count));
        }
        // Whole directory entries only.
        let start = (offset as usize).min(snap.len());
        let end = (start + count as usize).min(snap.len());
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
        let f = self.fid(fid)?.clone();
        let words = ndb::words(&String::from_utf8_lossy(data));
        match f.path {
            NEW => {
                let mine = self
                    .services
                    .iter()
                    .find(|(_, s)| s.by == Some(fid))
                    .map(|(p, _)| *p);
                match mine {
                    Some(p) => {
                        let new = pairs(&words)?;
                        if let Some(s) = self.services.get_mut(&p) {
                            Registry::set(&mut s.attrs, new);
                        }
                    }
                    None => {
                        let (name, rest) = words
                            .split_first()
                            .ok_or_else(|| String::from("no service name"))?;
                        if self.by_name(name).is_some()
                            || name.contains('/')
                            || name.starts_with('.')
                        {
                            return Err(format!("service {name} already registered"));
                        }
                        let mut attrs = Vec::new();
                        Registry::set(&mut attrs, pairs(rest)?);
                        let path = self.next;
                        self.next += 1;
                        self.services.insert(
                            path,
                            Service {
                                name: name.clone(),
                                attrs,
                                owner: f.uname.clone(),
                                by: Some(fid),
                            },
                        );
                    }
                }
                self.changed();
            }
            FIND => {
                let mut filter = Vec::new();
                for p in words.chunks(2) {
                    let v = p.get(1).cloned().unwrap_or_else(|| String::from("*"));
                    filter.push((p[0].clone(), v));
                }
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.filter = filter;
                    f.snap.clear();
                }
            }
            p if p >= SERVICE => {
                let new = pairs(&words)?;
                let s = self.services.get_mut(&p).ok_or(error::ENONEXIST)?;
                if s.owner != f.uname {
                    return Err(String::from(error::EPERM));
                }
                Registry::set(&mut s.attrs, new);
                self.changed();
            }
            _ => return Err(String::from(error::EPERM)),
        }
        Ok(data.len() as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let gone: Vec<u64> = self
            .services
            .iter()
            .filter(|(_, s)| s.by == Some(fid))
            .map(|(p, _)| *p)
            .collect();
        for p in gone {
            let persist = self.services[&p]
                .attrs
                .iter()
                .any(|(a, v)| a == "persist" && v != "0" && !v.is_empty());
            if persist {
                if let Some(s) = self.services.get_mut(&p) {
                    s.by = None;
                }
            } else {
                self.services.remove(&p);
                self.changed();
            }
        }
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let f = self.fid(fid)?.clone();
        let allowed = f.path >= SERVICE
            && self
                .services
                .get(&f.path)
                .is_some_and(|s| s.owner == f.uname);
        self.clunk(fid)?;
        if !allowed {
            return Err(String::from(error::EPERM));
        }
        self.services.remove(&f.path);
        self.changed();
        Ok(())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let path = self.fid(fid)?.path;
        let mut d = self.dir(path).ok_or(error::ENONEXIST)?;
        if path >= SERVICE || path == INDEX {
            d.length = self.text(path, None).len() as u64;
        }
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "registry [-f dbfile]");
    let mut file = None;
    while let Some(c) = arg.opt() {
        match c {
            'f' => file = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error::from("usage: registry [-f dbfile]"));
    }
    let mut reg = Registry {
        services: BTreeMap::new(),
        fids: BTreeMap::new(),
        next: SERVICE,
        vers: 0,
        iounit: 8192,
    };
    if let Some(f) = file {
        let db = ndb::open(&f, &|f| ndb::read_std(f, &purgatorio::errstr)).map_err(Error)?;
        for e in &db {
            let Some(name) = e.get("service") else {
                continue;
            };
            let attrs = e
                .pairs()
                .filter(|p| p.attr != "service")
                .map(|p| (p.attr.clone(), p.val.clone()))
                .collect();
            let path = reg.next;
            reg.next += 1;
            reg.services.insert(
                path,
                Service {
                    name: name.to_string(),
                    attrs,
                    owner: String::from("registry"),
                    by: None,
                },
            );
        }
    }
    // SAFETY: fd 0 is the Styx connection for the life of the prog.
    let io = unsafe { std::fs::File::from_raw_fd(0) };
    styx::serve(io, &mut reg)
}
