//! A small synthetic file server: keyfs and factotum describe their files
//! with [`Files`], and [`Server`] speaks Styx for them.

use std::collections::BTreeMap;

use purgatorio::abi::error;
use purgatorio::styx::StyxFs;
use purgatorio::{DMDIR, Dir, Qid};

/// A path below the root: `[]` is the root, `["alice", "secret"]` a file.
pub type Path = Vec<String>;

/// The files a server offers. `fid` lets a file keep per-open state (the
/// factotum's rpc conversation).
pub trait Files {
    /// `Some(true)` for a directory, `Some(false)` for a file, `None` when
    /// there is nothing at `path`.
    fn lookup(&self, path: &[String]) -> Option<bool>;
    /// The names in directory `path`.
    fn list(&self, path: &[String]) -> Vec<String>;
    /// Permission bits (without `DMDIR`).
    fn perm(&self, path: &[String]) -> u32 {
        if self.lookup(path) == Some(true) {
            0o700
        } else {
            0o600
        }
    }
    fn open(&mut self, _path: &[String], _fid: u32, _omode: u8) -> Result<(), String> {
        Ok(())
    }
    fn read(
        &mut self,
        path: &[String],
        fid: u32,
        offset: u64,
        count: u32,
    ) -> Result<Vec<u8>, String>;
    fn write(&mut self, path: &[String], fid: u32, offset: u64, data: &[u8])
    -> Result<u32, String>;
    /// Creates `name` in directory `dir`.
    fn create(&mut self, _dir: &[String], _name: &str, _perm: u32) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
    fn remove(&mut self, _path: &[String]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
    fn clunk(&mut self, _path: &[String], _fid: u32) {}
}

struct Fid {
    path: Path,
    open: bool,
    /// Packed directory entries, rebuilt when a read starts at offset 0.
    snap: Vec<u8>,
}

pub struct Server<F: Files> {
    pub files: F,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
    owner: String,
}

/// A stable qid path for `path` (FNV-1a over its elements).
fn qpath(path: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in path {
        for b in p.bytes().chain([0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

impl<F: Files> Server<F> {
    pub fn new(files: F, owner: &str) -> Server<F> {
        Server {
            files,
            fids: BTreeMap::new(),
            iounit: 8192,
            owner: String::from(owner),
        }
    }

    fn qid(&self, path: &[String]) -> Result<Qid, String> {
        let dir = self
            .files
            .lookup(path)
            .ok_or_else(|| String::from(error::ENONEXIST))?;
        Ok(Qid {
            path: qpath(path),
            vers: 0,
            qtype: if dir {
                purg_styx::QTDIR
            } else {
                purg_styx::QTFILE
            },
        })
    }

    fn dir(&self, path: &[String]) -> Result<Dir, String> {
        let qid = self.qid(path)?;
        let isdir = self.files.lookup(path) == Some(true);
        Ok(Dir {
            qid,
            mode: self.files.perm(path) | if isdir { DMDIR } else { 0 },
            name: path.last().cloned().unwrap_or_else(|| String::from(".")),
            uid: self.owner.clone(),
            gid: self.owner.clone(),
            muid: self.owner.clone(),
            ..Dir::default()
        })
    }

    fn path(&self, fid: u32) -> Result<Path, String> {
        self.fids
            .get(&fid)
            .map(|f| f.path.clone())
            .ok_or_else(|| String::from("unknown fid"))
    }
}

impl<F: Files> StyxFs for Server<F> {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(purgatorio::styx::IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                path: Vec::new(),
                open: false,
                snap: Vec::new(),
            },
        );
        self.qid(&[])
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let mut path = self.path(fid)?;
        let mut walked = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let mut next = path.clone();
            if *name == ".." {
                next.pop();
            } else {
                next.push(String::from(*name));
            }
            if self.files.lookup(&path) != Some(true) || self.files.lookup(&next).is_none() {
                if i == 0 {
                    return Err(String::from(error::ENONEXIST));
                }
                return Ok(walked);
            }
            walked.push(self.qid(&next)?);
            path = next;
        }
        self.fids.insert(
            newfid,
            Fid {
                path,
                open: false,
                snap: Vec::new(),
            },
        );
        Ok(walked)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let path = self.path(fid)?;
        let qid = self.qid(&path)?;
        self.files.open(&path, fid, omode)?;
        if let Some(f) = self.fids.get_mut(&fid) {
            f.open = true;
        }
        Ok((qid, self.iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, omode: u8) -> Result<(Qid, u32), String> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(String::from(error::EBADARG));
        }
        let dir = self.path(fid)?;
        if self.files.lookup(&dir) != Some(true) {
            return Err(String::from(error::ENOTDIR));
        }
        let mut path = dir.clone();
        path.push(String::from(name));
        if self.files.lookup(&path).is_some() {
            return Err(String::from(error::EEXIST));
        }
        self.files.create(&dir, name, perm)?;
        self.files.open(&path, fid, omode)?;
        let qid = self.qid(&path)?;
        self.fids.insert(
            fid,
            Fid {
                path,
                open: true,
                snap: Vec::new(),
            },
        );
        Ok((qid, self.iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let path = self.path(fid)?;
        if self.files.lookup(&path) != Some(true) {
            return self.files.read(&path, fid, offset, count);
        }
        if offset == 0 {
            let mut snap = Vec::new();
            for name in self.files.list(&path) {
                let mut child = path.clone();
                child.push(name);
                if let Ok(d) = self.dir(&child) {
                    let _ = d.pack_into(&mut snap);
                }
            }
            if let Some(f) = self.fids.get_mut(&fid) {
                f.snap = snap;
            }
        }
        let snap = self.fids.get(&fid).map(|f| &f.snap[..]).unwrap_or(&[]);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= snap.len() {
            return Ok(Vec::new());
        }
        let end = (start + count as usize).min(snap.len());
        // Return only whole entries.
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

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let path = self.path(fid)?;
        if self.files.lookup(&path) == Some(true) {
            return Err(String::from(error::EISDIR));
        }
        self.files.write(&path, fid, offset, data)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if f.open {
            self.files.clunk(&f.path, fid);
        }
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let path = self.path(fid)?;
        let r = self.files.remove(&path);
        let _ = self.clunk(fid);
        r
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let path = self.path(fid)?;
        self.dir(&path)?.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

/// Serves `files` in a new prog and mounts it at `mtpt`. The server is
/// this same command run with `serve_flag` and `extra` arguments; it calls
/// [`run`] with its own `Files`. `secret` is written to the server on its
/// fd 1 before the mount (keyfs's master key), so it is never an argument.
pub fn start(
    cmd: &str,
    serve_flag: &str,
    extra: &[String],
    mtpt: &str,
    secret: &[u8],
) -> Result<(), purgatorio::Error> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let [a, b] = purgatorio::pipe()?;
    let [kr, kw] = purgatorio::pipe()?;
    let mut argv = vec![String::from(cmd), String::from(serve_flag)];
    argv.extend_from_slice(extra);
    purgatorio::spawn(
        &format!("/bin/{cmd}.wasm"),
        &argv,
        purgatorio::abi::pctl::NEWFD,
        &[(a.as_raw_fd(), 0), (kr.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(a);
    drop(kr);
    let mut kw = std::fs::File::from(kw);
    kw.write_all(secret)?;
    drop(kw);
    purgatorio::mount(
        b.as_raw_fd(),
        -1,
        mtpt,
        purgatorio::abi::mount::MREPL | purgatorio::abi::mount::MCREATE,
        "",
    )?;
    Ok(())
}

/// The server half of [`start`]: reads the secret from fd 1, builds the
/// files from it, and speaks Styx on fd 0.
pub fn run<F: Files>(
    make: impl FnOnce(Vec<u8>) -> Result<F, purgatorio::Error>,
) -> Result<(), purgatorio::Error> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    let mut secret = Vec::new();
    // SAFETY: fds 0 and 1 were given to this prog by `start`.
    let mut k = unsafe { std::fs::File::from_raw_fd(1) };
    k.read_to_end(&mut secret)?;
    drop(k);
    let files = make(secret)?;
    let io = unsafe { std::fs::File::from_raw_fd(0) };
    let mut srv = Server::new(files, &crate::user());
    purgatorio::styx::serve(io, &mut srv)
}
