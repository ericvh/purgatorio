//! ftpfs [-/dpq] [-m mountpoint] [-a password] ftphost: mount the files of
//! an FTP server on `mountpoint` (default `/n/ftp`). The tree is the
//! login's home directory, or with `-/` the server's root. With `-a` the
//! login is `anonymous` with that password; otherwise ftpfs asks for a
//! user and password on standard input. Data goes over passive
//! connections, or with `-p` active ones. `-d` writes the conversation
//! on standard error, and `-q` keeps the server's messages quiet.
//!
//! Directory listings (`LIST`, Unix or DOS form) are kept until the
//! directory changes over this connection; walking to `.flush.ftpfs` in
//! a directory forgets its listing. A file is read whole when opened and
//! written whole (`STOR`) when closed. `mkdir` is `MKD`, `rm` is `DELE`
//! or `RMD`, and a wstat of the name renames (`RNFR`/`RNTO`); other
//! changes of mode or time are accepted and ignored. The connection ends
//! when the mount goes away.
//!
//! Follows Inferno's ftpfs(4). Where it differs: renaming works; cached
//! files are kept in memory, not in /tmp. Not here: symbolic links
//! (they show as files), and resuming a broken connection.

mod ftp;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

use ftp::{Entry, Ftp};
use purg_styx::{IOHDRSZ, QTDIR, QTFILE, Qid};
use purgatorio::abi::mount::{MCREATE, MNOAUTH, MREPL};
use purgatorio::abi::{error, mode, pctl};
use purgatorio::arg::Arg;
use purgatorio::styx::StyxFs;
use purgatorio::{Context, DMDIR, Dir, Error, daytime, dial};

const USAGE: &str = "ftpfs [-/dpq] [-m mountpoint] [-a password] ftphost";
const FLUSH: &str = ".flush.ftpfs";
/// The child's control connection.
const CTL_FD: i32 = 5;

struct Fid {
    path: String,
    isdir: bool,
    open: Option<u8>,
    /// A file's contents while it is open.
    data: Vec<u8>,
    /// Whether `data` must be stored when the fid is clunked.
    dirty: bool,
    snap: Vec<u8>,
}

struct FtpFs {
    ftp: Ftp,
    root: String,
    lists: BTreeMap<String, Vec<Entry>>,
    qids: BTreeMap<String, u64>,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some(("", n)) => ("/", n),
        Some((p, n)) => (p, n),
        None => ("/", path),
    }
}

/// An error string for a failed reply.
fn failed(r: &ftp::Reply, missing: bool) -> String {
    match r.code {
        0 => r.text.clone(),
        550 if missing => String::from(error::ENONEXIST),
        550 | 553 | 530 | 532 => String::from(error::EPERM),
        452 | 552 => String::from("file system full"),
        _ => format!("ftp: {} {}", r.code, r.text),
    }
}

impl FtpFs {
    fn qid(&mut self, path: &str, isdir: bool) -> Qid {
        let n = self.qids.len() as u64;
        let id = *self.qids.entry(String::from(path)).or_insert(n);
        Qid {
            path: id,
            vers: 0,
            qtype: if isdir { QTDIR } else { QTFILE },
        }
    }

    /// The entries of the directory `path`, from the cache or the server.
    fn list(&mut self, path: &str) -> Result<Vec<Entry>, String> {
        if let Some(l) = self.lists.get(path) {
            return Ok(l.clone());
        }
        let r = self.ftp.cmd(&format!("CWD {path}"))?;
        if r.code / 100 != 2 {
            return Err(failed(&r, true));
        }
        let text = self
            .ftp
            .transfer("LIST", None)
            .map_err(|r| failed(&r, true))?;
        let now = daytime::now();
        let mut out: Vec<Entry> = String::from_utf8_lossy(&text)
            .lines()
            .filter_map(|l| ftp::parse_list(l, now))
            .filter(|e| e.name != "." && e.name != ".." && !e.name.contains('/'))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        self.lists.insert(String::from(path), out.clone());
        Ok(out)
    }

    fn forget(&mut self, dir: &str) {
        self.lists.remove(dir);
    }

    fn entry(&mut self, path: &str) -> Result<Option<Entry>, String> {
        if path == self.root {
            return Ok(None);
        }
        let (dir, name) = parent(path);
        let dir = String::from(dir);
        self.list(&dir)?
            .into_iter()
            .find(|e| e.name == name)
            .map(Some)
            .ok_or_else(|| String::from(error::ENONEXIST))
    }

    fn dir_of(&mut self, path: &str, e: Option<&Entry>) -> Dir {
        let isdir = e.is_none_or(|e| e.dir);
        let qid = self.qid(path, isdir);
        match e {
            None => Dir {
                qid,
                mode: DMDIR | 0o777,
                name: String::from("/"),
                uid: String::from("ftp"),
                gid: String::from("ftp"),
                muid: String::from("ftp"),
                ..Dir::default()
            },
            Some(e) => Dir {
                qid,
                mode: e.perm | if e.dir { DMDIR } else { 0 },
                atime: e.mtime,
                mtime: e.mtime,
                length: if e.dir { 0 } else { e.size },
                name: e.name.clone(),
                uid: e.uid.clone(),
                gid: e.gid.clone(),
                muid: e.uid.clone(),
                ..Dir::default()
            },
        }
    }

    fn fid(&mut self, fid: u32) -> Result<&mut Fid, String> {
        self.fids
            .get_mut(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn retr(&mut self, path: &str) -> Result<Vec<u8>, String> {
        self.ftp
            .transfer(&format!("RETR {path}"), None)
            .map_err(|r| failed(&r, true))
    }

    fn stor(&mut self, path: &str, data: &[u8]) -> Result<(), String> {
        let res = self
            .ftp
            .transfer(&format!("STOR {path}"), Some(data))
            .map(drop)
            .map_err(|r| failed(&r, false));
        self.forget(parent(path).0);
        res
    }
}

impl StyxFs for FtpFs {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        let root = self.root.clone();
        self.fids.insert(
            fid,
            Fid {
                path: root.clone(),
                isdir: true,
                open: None,
                data: Vec::new(),
                dirty: false,
                snap: Vec::new(),
            },
        );
        Ok(self.qid(&root, true))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let f = self.fid(fid)?;
        let (mut path, mut isdir) = (f.path.clone(), f.isdir);
        let mut qids = Vec::new();
        for name in names {
            let step = if !isdir {
                Err(String::from(error::ENOTDIR))
            } else if *name == ".." {
                if path != self.root {
                    path = String::from(parent(&path).0);
                }
                Ok(())
            } else if *name == FLUSH {
                self.forget(&path);
                Err(String::from(error::ENONEXIST))
            } else {
                let p = join(&path, name);
                match self.entry(&p) {
                    Ok(e) => {
                        isdir = e.is_none_or(|e| e.dir);
                        path = p;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            };
            match step {
                Ok(()) => {
                    let q = self.qid(&path, isdir);
                    qids.push(q);
                }
                Err(e) if qids.is_empty() => return Err(e),
                Err(_) => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                path,
                isdir,
                open: None,
                data: Vec::new(),
                dirty: false,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let f = self.fid(fid)?;
        let (path, isdir) = (f.path.clone(), f.isdir);
        let access = omode & mode::ACCESS;
        let writes = matches!(access, mode::OWRITE | mode::ORDWR) || omode & mode::OTRUNC != 0;
        if isdir && writes {
            return Err(String::from(error::EPERM));
        }
        let mut data = Vec::new();
        let mut dirty = false;
        if !isdir {
            if omode & mode::OTRUNC != 0 {
                dirty = true;
            } else {
                data = self.retr(&path)?;
            }
        }
        let q = self.qid(&path, isdir);
        let f = self.fid(fid)?;
        f.open = Some(omode);
        f.data = data;
        f.dirty = dirty;
        Ok((q, self.iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, omode: u8) -> Result<(Qid, u32), String> {
        let f = self.fid(fid)?;
        if !f.isdir {
            return Err(String::from("create in non-directory"));
        }
        if f.open.is_some() {
            return Err(String::from("create on open file"));
        }
        if name == "." || name == ".." || name == FLUSH {
            return Err(String::from("file name syntax"));
        }
        let dir = f.path.clone();
        let path = join(&dir, name);
        if self.entry(&path).is_ok() {
            return Err(String::from(error::EEXIST));
        }
        let isdir = perm & DMDIR != 0;
        if isdir {
            let r = self.ftp.cmd(&format!("MKD {path}"))?;
            self.forget(&dir);
            if r.code / 100 != 2 {
                return Err(failed(&r, false));
            }
        } else {
            // Made now, so it is there even if nothing is written.
            self.stor(&path, &[])?;
        }
        let q = self.qid(&path, isdir);
        let f = self.fid(fid)?;
        f.path = path;
        f.isdir = isdir;
        f.open = Some(omode);
        f.data = Vec::new();
        f.dirty = false;
        Ok((q, self.iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        if f.open.is_none() {
            return Err(String::from("fid not open"));
        }
        if f.isdir {
            if offset == 0 {
                let path = f.path.clone();
                let mut snap = Vec::new();
                for e in self.list(&path)? {
                    let d = self.dir_of(&join(&path, &e.name), Some(&e));
                    let _ = d.pack_into(&mut snap);
                }
                self.fid(fid)?.snap = snap;
            }
            return Ok(styxserv::rotree::dir_slice(
                &self.fid(fid)?.snap,
                offset,
                count,
            ));
        }
        let start = (offset as usize).min(f.data.len());
        let end = start.saturating_add(count as usize).min(f.data.len());
        Ok(f.data[start..end].to_vec())
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let f = self.fid(fid)?;
        if !f
            .open
            .is_some_and(|m| matches!(m & mode::ACCESS, mode::OWRITE | mode::ORDWR))
        {
            return Err(String::from("fid not open for write"));
        }
        let off = usize::try_from(offset).map_err(|_| String::from("offset too big"))?;
        let end = off + data.len();
        if f.data.len() < end {
            f.data.resize(end, 0);
        }
        f.data[off..end].copy_from_slice(data);
        f.dirty = true;
        Ok(data.len() as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let res = if f.dirty {
            self.stor(&f.path, &f.data)
        } else {
            Ok(())
        };
        if f.open.is_some_and(|m| m & mode::ORCLOSE != 0) {
            let r = self.ftp.cmd(&format!("DELE {}", f.path))?;
            self.forget(parent(&f.path).0);
            if r.code / 100 != 2 {
                return Err(failed(&r, false));
            }
        }
        res
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if f.path == self.root {
            return Err(String::from(error::EPERM));
        }
        let cmd = if f.isdir { "RMD" } else { "DELE" };
        let r = self.ftp.cmd(&format!("{cmd} {}", f.path))?;
        self.forget(parent(&f.path).0);
        if f.isdir {
            self.forget(&f.path);
        }
        if r.code / 100 != 2 {
            return Err(failed(&r, false));
        }
        Ok(())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let path = self.fid(fid)?.path.clone();
        let e = self.entry(&path)?;
        let mut d = self.dir_of(&path, e.as_ref());
        let f = self.fid(fid)?;
        if f.dirty {
            d.length = f.data.len() as u64;
        }
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        let (d, _) = Dir::unpack(stat).map_err(|e| e.to_string())?;
        let f = self.fid(fid)?;
        let path = f.path.clone();
        if d.length == 0 && !f.isdir {
            f.data.clear();
            f.dirty = true;
            if f.open.is_none() {
                self.fid(fid)?.dirty = false;
                self.stor(&path, &[])?;
            }
        }
        let (dir, name) = parent(&path);
        if !d.name.is_empty() && d.name != name {
            if path == self.root || d.name.contains('/') {
                return Err(String::from(error::EPERM));
            }
            let dir = String::from(dir);
            let to = join(&dir, &d.name);
            let r = self.ftp.cmd(&format!("RNFR {path}"))?;
            if r.code / 100 != 3 {
                return Err(failed(&r, true));
            }
            let r = self.ftp.cmd(&format!("RNTO {to}"))?;
            self.forget(&dir);
            if r.code / 100 != 2 {
                return Err(failed(&r, false));
            }
            for f in self.fids.values_mut() {
                if f.path == path {
                    f.path = to.clone();
                } else if let Some(rest) = f.path.strip_prefix(&format!("{path}/")) {
                    f.path = format!("{to}/{rest}");
                }
            }
            let stale: Vec<String> = self
                .lists
                .keys()
                .filter(|k| k.starts_with(&format!("{path}/")) || **k == path)
                .cloned()
                .collect();
            for k in stale {
                self.lists.remove(&k);
            }
        }
        Ok(())
    }
}

/// Logs in on the control connection; returns the home directory.
fn login(ftp: &mut Ftp, user: &str, pass: &str, quiet: bool) -> Result<String, String> {
    let r = ftp.reply()?;
    if r.code != 220 {
        return Err(format!("ftp: {}", r.text));
    }
    if !quiet {
        println!("{}", r.text);
    }
    let mut r = ftp.cmd(&format!("USER {user}"))?;
    if r.code == 331 {
        r = ftp.cmd(&format!("PASS {pass}"))?;
    }
    if r.code / 100 != 2 {
        return Err(format!("login failed: {}", r.text));
    }
    if !quiet {
        println!("{}", r.text);
    }
    let r = ftp.cmd("TYPE I")?;
    if r.code / 100 != 2 {
        return Err(format!("ftp: {}", r.text));
    }
    let r = ftp.cmd("PWD")?;
    let home = r
        .text
        .split('"')
        .nth(1)
        .filter(|_| r.code == 257)
        .unwrap_or("/");
    Ok(String::from(home))
}

fn prompt(what: &str) -> Result<String, Error> {
    print!("{what}: ");
    std::io::stdout().flush().map_err(Error::from)?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(Error::from)?;
    Ok(String::from(line.trim_end_matches(['\r', '\n'])))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let [_, _, flags, dir, root] = args.as_slice() else {
            return Err(Error::from("bad --serve arguments"));
        };
        // SAFETY: the parent gave this prog the control connection as
        // CTL_FD, and nothing else uses it.
        let conn = unsafe { File::from_raw_fd(CTL_FD) };
        let ftp = Ftp::new(conn, dir.clone(), flags.contains('p'), flags.contains('d'));
        let mut fs = FtpFs {
            ftp,
            root: root.clone(),
            lists: BTreeMap::new(),
            qids: BTreeMap::new(),
            fids: BTreeMap::new(),
            iounit: 8192,
        };
        let res = styxserv::bg::serve(&mut fs);
        let _ = fs.ftp.send("QUIT");
        return res;
    }
    let mut arg = Arg::new(args, USAGE);
    let mut mtpt = String::from("/n/ftp");
    let mut password = None;
    let mut flags = String::from("-");
    while let Some(c) = arg.opt() {
        match c {
            'm' => mtpt = arg.earg()?,
            'a' => password = Some(arg.earg()?),
            '/' | 'd' | 'p' | 'q' => flags.push(c),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [host] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let addr = match host.split('!').count() {
        1 => format!("tcp!{host}!ftp"),
        2 => format!("tcp!{host}"),
        _ => host.clone(),
    };
    let conn = dial::dial(&addr).map_err(|e| Error(format!("cannot dial {addr}: {e}")))?;
    let data = File::from(conn.data.expect("a connected conversation"));
    let mut ftp = Ftp::new(data, conn.dir.clone(), false, flags.contains('d'));
    let (user, pass) = match password {
        Some(p) => (String::from("anonymous"), p),
        None => (prompt("User")?, prompt("Password")?),
    };
    let home = login(&mut ftp, &user, &pass, flags.contains('q')).map_err(Error)?;
    let root = if flags.contains('/') {
        String::from("/")
    } else {
        home
    };
    let [a, b] = purgatorio::pipe()?;
    let ctl = ftp.into_file().into_raw_fd();
    let argv = [
        String::from("ftpfs"),
        String::from("--serve"),
        flags,
        conn.dir.clone(),
        root,
    ];
    purgatorio::spawn(
        "/bin/ftpfs.wasm",
        &argv,
        pctl::NEWFD | pctl::FORKNS,
        &[
            (a.as_raw_fd(), 0),
            (2, 2),
            (ctl, CTL_FD),
            (conn.ctl.as_raw_fd(), 6),
        ],
    )?;
    drop(a);
    // SAFETY: `ctl` came from `into_raw_fd` above and is closed once.
    drop(unsafe { File::from_raw_fd(ctl) });
    purgatorio::mount(b.as_raw_fd(), -1, &mtpt, MREPL | MCREATE | MNOAUTH, "")
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")))?;
    Ok(())
}
