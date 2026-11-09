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
/// When `noauth` is false and the exported tree holds a public key, `Tattach`
/// is accepted only after a signature over a challenge from `platform`.
pub fn serve(
    pgrp: Arc<Pgrp>,
    root: &str,
    fd: &Chan,
    platform: &dyn Platform,
    noauth: bool,
) -> KResult<()> {
    let mut fs = Export::new(pgrp, root)?;
    let mut pubs = Vec::new();
    if !noauth {
        push_key(
            &mut pubs,
            &fs.pgrp,
            &under(&fs.root, "lib/test/auth.key.pub"),
        );
    }
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
            match auth_reply(&mut afids, &mut pubs, platform, &fs, &msg) {
                Some(bytes) => bytes,
                None => dispatch(&mut fs, &msg, &mut msize),
            }
        };
        write_full(fd, &reply)?;
    }
}

struct Afid {
    challenge: [u8; crate::auth::CHALLENGE],
    got: Vec<u8>,
    ok: bool,
}

/// Reads a 32-byte key file. Anything else, including a missing file, is
/// `None`.
pub(crate) fn read_key(pgrp: &Pgrp, path: &str) -> Option<[u8; crate::auth::PUBLIC]> {
    let c = pgrp.namec(path, "/").ok()?;
    let c = open(c, mode::OREAD).ok()?;
    let mut buf = [0u8; crate::auth::PUBLIC + 1];
    let n = c.dev.read(&c, &mut buf, 0).ok()?;
    let bytes: [u8; crate::auth::PUBLIC] = buf.get(..n)?.try_into().ok()?;
    if c.dev.read(&c, &mut buf, n as u64).ok()? != 0 {
        return None;
    }
    Some(bytes)
}

fn under(root: &str, rel: &str) -> String {
    if root == "/" {
        alloc::format!("/{rel}")
    } else {
        alloc::format!("{root}/{rel}")
    }
}

fn push_key(pubs: &mut Vec<[u8; crate::auth::PUBLIC]>, pgrp: &Pgrp, path: &str) {
    let Some(key) = read_key(pgrp, path) else {
        return;
    };
    if !pubs.contains(&key) {
        pubs.push(key);
    }
}

fn user_pub(root: &str, uname: &str) -> Option<String> {
    if uname.is_empty() || uname == "." || uname == ".." || uname.contains('/') {
        return None;
    }
    Some(under(root, &alloc::format!("usr/{uname}/key.pub")))
}

fn reply(tag: u16, fcall: Fcall<'_>) -> Vec<u8> {
    Msg { tag, fcall }.encode().unwrap_or_else(|_| Vec::new())
}

fn rerr(tag: u16, ename: &'static str) -> Vec<u8> {
    reply(tag, Fcall::Rerror { ename })
}

/// Handles `Tauth` and the auth fid when this export has a public key.
/// `None` means the message belongs to the file tree.
fn auth_reply(
    afids: &mut BTreeMap<u32, Afid>,
    pubs: &mut Vec<[u8; crate::auth::PUBLIC]>,
    platform: &dyn Platform,
    fs: &Export,
    msg: &Msg<'_>,
) -> Option<Vec<u8>> {
    if let Fcall::Tauth { uname, .. } = &msg.fcall
        && let Some(path) = user_pub(&fs.root, uname)
    {
        push_key(pubs, &fs.pgrp, &path);
    }
    if pubs.is_empty() {
        return None;
    }
    let tag = msg.tag;
    if let Fcall::Tauth { afid, .. } = &msg.fcall {
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
                got: Vec::new(),
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
        let n = u32::try_from(data.len()).unwrap_or(u32::MAX);
        if afid.got.len().saturating_add(data.len()) > crate::auth::PROOF {
            afid.got.clear();
            afid.ok = false;
            return Some(rerr(tag, crate::auth::FAILED));
        }
        afid.got.extend_from_slice(data);
        if afid.got.len() == crate::auth::PROOF {
            let challenge = afid.challenge;
            let got = afid.got.clone();
            let ok = crate::auth::accept(pubs, &challenge, &got);
            afid.ok = ok;
            if !ok {
                afid.got.clear();
                return Some(rerr(tag, crate::auth::FAILED));
            }
        }
        return Some(reply(tag, Fcall::Rwrite { count: n }));
    }
    if let Fcall::Tclunk { fid } = &msg.fcall
        && afids.remove(fid).is_some()
    {
        return Some(reply(tag, Fcall::Rclunk));
    }
    if let Fcall::Tattach { afid, .. } = &msg.fcall {
        if afids.get(afid).is_some_and(|a| a.ok) {
            return None;
        }
        return Some(rerr(tag, crate::auth::REQUIRED));
    }
    None
}

struct Export {
    pgrp: Arc<Pgrp>,
    root: String,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
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
        let f = self
            .fids
            .remove(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
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
