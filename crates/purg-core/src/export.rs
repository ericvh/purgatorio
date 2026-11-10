//! `export`: serve a namespace over a file descriptor as a Styx server.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::Platform;
use purg_styx::server::{StyxFs, dispatch};
use purg_styx::{Dir, Fcall, Msg, NOFID, QTAUTH, Qid};

use crate::chan::Chan;
use crate::error::{Error, Result as KResult};
use crate::names::cleanname;
use crate::pgrp::{Pgrp, absolute};
use crate::rw::{read_msg, write_full};
use crate::sysfile::{self, open};

struct Fid {
    path: String,
    chan: Chan,
}

/// Serves `root` (a path in `pgrp`) as 9P2000 on `fd` until the client hangs up.
///
/// With `rdonly`, nothing in the tree can be changed through the connection.
///
/// When `noauth` is false and `pgrp` (the exporter's namespace) trusts a
/// key or a signer (`docs/auth.md`), `Tattach` is accepted only after a
/// proof over a challenge from `platform`.
pub fn serve(
    pgrp: Arc<Pgrp>,
    root: &str,
    fd: &Chan,
    platform: &dyn Platform,
    noauth: bool,
    rdonly: bool,
) -> KResult<()> {
    let mut fs = Export::new(pgrp, root)?;
    fs.rdonly = rdonly;
    let mut afids = BTreeMap::new();
    let mut msize = purg_styx::MAX_MSIZE;
    loop {
        let buf = match read_msg(fd, msize) {
            Ok(b) => b,
            Err(e) if e.as_str() == error::EHUNGUP || e.as_str() == error::EINTR => return Ok(()),
            Err(e) => return Err(e),
        };
        let msg = match Msg::decode(&buf, msize) {
            Ok(m) => m,
            Err(_) => return Err(Error::new(error::EIO)),
        };
        let reply = if noauth {
            dispatch(&mut fs, &msg, &mut msize)
        } else {
            match auth_reply(&mut afids, platform, &fs, &msg) {
                Some(bytes) => bytes,
                None => dispatch(&mut fs, &msg, &mut msize),
            }
        };
        write_full(fd, &reply)?;
    }
}

struct Afid {
    challenge: [u8; crate::auth::CHALLENGE],
    uname: String,
    aname: String,
    ok: bool,
}

/// What the exporter trusts for one user: keys it holds directly,
/// signers, and revoked keys (`None` when that list does not parse).
struct Trust {
    keys: Vec<[u8; crate::auth::PUBLIC]>,
    signers: Vec<[u8; crate::auth::PUBLIC]>,
    revoked: Option<Vec<[u8; crate::auth::PUBLIC]>>,
}

/// The longest key list read.
const MAX_LIST: usize = 64 * 1024;
/// Trusted signers and revoked keys, in the exporter's namespace.
const SIGNERS: &str = "/lib/keys/signers";
const REVOKED: &str = "/lib/keys/revoked";

/// Reads the trust material for `uname` from the exporter's own namespace
/// (`fs.pgrp`), at absolute paths. Nothing is read from under the exported
/// tree, so a client that can write there cannot add a key or a signer.
fn trust(fs: &Export, uname: &str) -> Trust {
    let mut keys = Vec::new();
    let mut paths = alloc::vec![String::from("/lib/test/auth.key.pub")];
    if crate::auth::safe_user(uname) {
        paths.push(alloc::format!("/usr/{uname}/key.pub"));
    }
    for path in paths {
        if let Some(key) = crate::auth::read_key(&fs.pgrp, &path)
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    let signers = crate::auth::read_file(&fs.pgrp, SIGNERS, MAX_LIST)
        .map(|t| purg_auth::keys::parse_list_lenient(&t))
        .unwrap_or_default();
    // A missing list revokes nothing. One that is there but cannot be read
    // or parsed fails closed.
    let revoked = match crate::auth::read_file(&fs.pgrp, REVOKED, MAX_LIST) {
        Some(t) => purg_auth::keys::parse_list(&t),
        None if fs.pgrp.namec(REVOKED, "/").is_ok() => None,
        None => Some(Vec::new()),
    };
    Trust {
        keys,
        signers,
        revoked,
    }
}

impl Trust {
    fn policy(&self, now: u64) -> purg_auth::proof::Policy<'_> {
        purg_auth::proof::Policy {
            keys: &self.keys,
            signers: &self.signers,
            revoked: self.revoked.as_deref(),
            now,
        }
    }

    fn required(&self) -> bool {
        self.policy(0).requires_auth()
    }
}

fn reply(tag: u16, fcall: Fcall<'_>) -> Vec<u8> {
    Msg { tag, fcall }.encode().unwrap_or_else(|_| Vec::new())
}

fn rerr(tag: u16, ename: &'static str) -> Vec<u8> {
    reply(tag, Fcall::Rerror { ename })
}

/// Handles `Tauth`, the auth fid and `Tattach` when this export
/// authenticates. `None` means the message belongs to the file tree.
fn auth_reply(
    afids: &mut BTreeMap<u32, Afid>,
    platform: &dyn Platform,
    fs: &Export,
    msg: &Msg<'_>,
) -> Option<Vec<u8>> {
    let tag = msg.tag;
    if let Fcall::Tauth { afid, uname, aname } = &msg.fcall {
        if !trust(fs, uname).required() {
            return None;
        }
        let afid = *afid;
        if afid == NOFID || fs.fids.contains_key(&afid) || afids.contains_key(&afid) {
            return Some(rerr(tag, "fid in use"));
        }
        let mut challenge = [0u8; crate::auth::CHALLENGE];
        platform.entropy(&mut challenge);
        afids.insert(
            afid,
            Afid {
                challenge,
                uname: String::from(*uname),
                aname: String::from(*aname),
                ok: false,
            },
        );
        return Some(reply(
            tag,
            Fcall::Rauth {
                aqid: Qid {
                    path: 0,
                    vers: 0,
                    qtype: QTAUTH,
                },
            },
        ));
    }
    if let Fcall::Tread { fid, offset, count } = &msg.fcall
        && let Some(afid) = afids.get(fid)
    {
        let start = usize::try_from(*offset).unwrap_or(usize::MAX);
        let data = afid.challenge.get(start..).unwrap_or(&[]);
        let n = (*count as usize).min(data.len());
        return Some(reply(tag, Fcall::Rread { data: &data[..n] }));
    }
    if let Fcall::Twrite { fid, data, .. } = &msg.fcall
        && let Some(afid) = afids.get_mut(fid)
    {
        // The proof is one write.
        let now = platform.now_ns() / 1_000_000_000;
        let t = trust(fs, &afid.uname);
        let checked = purg_auth::proof::check(
            &t.policy(now),
            &afid.challenge,
            &afid.uname,
            &afid.aname,
            data,
        );
        afid.ok = checked.is_ok();
        return Some(match checked {
            Ok(()) => reply(
                tag,
                Fcall::Rwrite {
                    count: u32::try_from(data.len()).unwrap_or(u32::MAX),
                },
            ),
            Err(e) => rerr(tag, e),
        });
    }
    if let Fcall::Tclunk { fid } = &msg.fcall
        && afids.remove(fid).is_some()
    {
        return Some(reply(tag, Fcall::Rclunk));
    }
    if let Fcall::Tattach {
        afid, uname, aname, ..
    } = &msg.fcall
    {
        return match afids.get(afid) {
            Some(a) if a.ok && a.uname == *uname && a.aname == *aname => None,
            Some(_) => Some(rerr(tag, crate::auth::REQUIRED)),
            None if trust(fs, uname).required() => Some(rerr(tag, crate::auth::REQUIRED)),
            None => None,
        };
    }
    None
}

struct Export {
    pgrp: Arc<Pgrp>,
    root: String,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
    /// Refuse every request that would change the tree.
    rdonly: bool,
}

impl Export {
    fn new(pgrp: Arc<Pgrp>, root: &str) -> KResult<Export> {
        let chan = pgrp.namec(root, "/")?;
        if !chan.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        Ok(Export {
            root: chan.name.clone(),
            pgrp,
            fids: BTreeMap::new(),
            iounit: 8192,
            rdonly: false,
        })
    }

    fn step(&self, path: &str, name: &str) -> KResult<Chan> {
        let next = if name == ".." {
            if path == self.root || path == "/" {
                self.root.clone()
            } else {
                match path.rsplit_once('/') {
                    Some((p, _)) if !p.is_empty() => {
                        let p = String::from(p);
                        if self.root != "/" && !p.starts_with(&self.root) {
                            self.root.clone()
                        } else {
                            p
                        }
                    }
                    _ => self.root.clone(),
                }
            }
        } else {
            let mut s = String::from(path);
            if !s.ends_with('/') {
                s.push('/');
            }
            s.push_str(name);
            cleanname(&s)
        };
        self.pgrp.namec(&next, "/")
    }

    /// Fails a request that changes the tree when the export is read-only.
    fn writable(&self) -> Result<(), String> {
        if self.rdonly {
            Err(String::from(error::EPERM))
        } else {
            Ok(())
        }
    }
}

impl StyxFs for Export {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(purg_styx::IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        let path = if aname.is_empty() {
            self.root.clone()
        } else {
            absolute(aname, &self.root)
        };
        let chan = self.pgrp.namec(&path, "/").map_err(|e| e.to_string())?;
        let qid = chan.qid;
        self.fids.insert(
            fid,
            Fid {
                path: chan.name.clone(),
                chan,
            },
        );
        Ok(qid)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let base = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let mut path = base.path.clone();
        let mut chan = base.chan.clone_unopened();
        if names.is_empty() {
            self.fids.insert(newfid, Fid { path, chan });
            return Ok(Vec::new());
        }
        let mut qids = Vec::new();
        for (i, name) in names.iter().enumerate() {
            match self.step(&path, name) {
                Ok(c) => {
                    qids.push(c.qid);
                    path = c.name.clone();
                    chan = c;
                }
                Err(e) => {
                    if i == 0 {
                        return Err(e.to_string());
                    }
                    return Ok(qids);
                }
            }
        }
        self.fids.insert(newfid, Fid { path, chan });
        Ok(qids)
    }

    fn open(&mut self, fid: u32, mode: u8) -> Result<(Qid, u32), String> {
        let access = mode & mode::ACCESS;
        if (access != mode::OREAD && access != mode::OEXEC)
            || mode & (mode::OTRUNC | mode::ORCLOSE) != 0
        {
            self.writable()?;
        }
        let f = self
            .fids
            .get_mut(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let opened = open(f.chan.clone_unopened(), mode).map_err(|e| e.to_string())?;
        let qid = opened.qid;
        f.chan = opened;
        Ok((qid, self.iounit))
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, mode: u8) -> Result<(Qid, u32), String> {
        self.writable()?;
        let path = {
            let f = self
                .fids
                .get(&fid)
                .ok_or_else(|| String::from("unknown fid"))?;
            let mut p = f.path.clone();
            if !p.ends_with('/') {
                p.push('/');
            }
            p.push_str(name);
            p
        };
        let (dir, leaf) = self
            .pgrp
            .create_target(&path, "/")
            .map_err(|e| e.to_string())?;
        let w = dir
            .dev
            .create(&dir, &leaf, perm)
            .map_err(|e| e.to_string())?;
        let mut c = Chan::new(dir.dev.clone(), dir.devno, w.qid, String::new(), w.aux);
        c.name = absolute(&path, "/");
        let opened = open(c, mode).map_err(|e| e.to_string())?;
        let qid = opened.qid;
        self.fids.insert(
            fid,
            Fid {
                path: opened.name.clone(),
                chan: opened,
            },
        );
        Ok((qid, self.iounit))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let mut buf = alloc::vec![0u8; count as usize];
        let n = if f.chan.is_dir() {
            sysfile::dir_read(&f.chan, &mut buf, offset)
        } else {
            f.chan.dev.read(&f.chan, &mut buf, offset)
        }
        .map_err(|e| e.to_string())?;
        buf.truncate(n);
        Ok(buf)
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        self.writable()?;
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let n = f
            .chan
            .dev
            .write(&f.chan, data, offset)
            .map_err(|e| e.to_string())?;
        u32::try_from(n).map_err(|_| String::from(error::EIO))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        // Tremove clunks the fid even when the remove is refused.
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        self.writable()?;
        f.chan.dev.remove(&f.chan).map_err(|e| e.to_string())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        let d = f.chan.dev.stat(&f.chan).map_err(|e| e.to_string())?;
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        self.writable()?;
        let d = Dir::unpack(stat)
            .map(|(d, _)| d)
            .map_err(|e| e.to_string())?;
        let f = self
            .fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        f.chan.dev.wstat(&f.chan, &d).map_err(|e| e.to_string())
    }
}
