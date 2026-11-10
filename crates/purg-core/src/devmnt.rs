//! `#M`: a Styx client. `mount` speaks 9P2000 on a file descriptor and returns
//! a Chan for the attached root. A reader kproc demultiplexes replies by tag.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use purg_abi::error;
use purg_abi::mode::ORDWR;
use purg_platform::sync::{Mutex, Note};
use purg_platform::{Platform, sync};
use purg_styx::server::{IOHDRSZ, MAX_MSIZE};
use purg_styx::{DMDIR, Dir, Fcall, Msg, NOFID, NOTAG, QTAUTH, QTDIR, VERSION, dirs};

use crate::auth;
use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};
use crate::mux::Mux;
use crate::rw::{read_msg, write_full};

struct Session {
    /// The mount id: `Chan::devno` of every file on this session.
    id: u32,
    fd: Arc<Chan>,
    mux: Arc<Mux>,
    note: Arc<Note>,
    msize: Arc<AtomicU32>,
    iounit: AtomicU32,
    write: Mutex<()>,
    next_fid: AtomicU32,
}

struct MntFile {
    session: Arc<Session>,
    fid: u32,
    live: AtomicBool,
    spec: String,
    /// Set when `Tcreate` already opened the fid, so the following `open`
    /// must not send `Topen`.
    fresh: AtomicBool,
}

impl Drop for MntFile {
    fn drop(&mut self) {
        if self.live.swap(false, Ordering::AcqRel) {
            let _ = self.session.clunk(self.fid);
        }
    }
}

impl Session {
    fn msize(&self) -> u32 {
        self.msize.load(Ordering::Acquire).max(IOHDRSZ)
    }

    fn iounit(&self) -> u32 {
        let n = self.iounit.load(Ordering::Acquire);
        if n == 0 {
            self.msize().saturating_sub(IOHDRSZ)
        } else {
            n
        }
    }

    fn spec_of(c: &Chan) -> String {
        c.aux::<MntFile>()
            .map(|f| f.spec.clone())
            .unwrap_or_default()
    }

    fn rpc(&self, fcall: Fcall<'_>) -> Result<Vec<u8>> {
        let tag = self.mux.begin()?;
        let bytes = Msg { tag, fcall }
            .encode()
            .map_err(|_| Error::new(error::EIO))?;
        if bytes.len() > self.msize() as usize {
            return Err(Error::new(error::EIO));
        }
        {
            let _g = self.write.lock();
            write_full(&self.fd, &bytes)?;
        }
        match self.mux.wait(tag) {
            Ok(buf) => Ok(buf),
            Err(e) if e.as_str() == error::EINTR => {
                self.flush_tag(tag);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    fn flush_tag(&self, tag: u16) {
        self.mux.flush(tag);
        let Ok(ftag) = self.mux.begin() else {
            return;
        };
        let Ok(bytes) = Msg {
            tag: ftag,
            fcall: Fcall::Tflush { oldtag: tag },
        }
        .encode() else {
            return;
        };
        {
            let _g = self.write.lock();
            let _ = write_full(&self.fd, &bytes);
        }
        let _ = self.mux.wait(ftag);
    }

    fn decode<'a>(&self, buf: &'a [u8]) -> Result<Fcall<'a>> {
        let msg = Msg::decode(buf, self.msize()).map_err(|_| Error::new(error::EIO))?;
        match msg.fcall {
            Fcall::Rerror { ename } => Err(Error::from(String::from(ename))),
            other => Ok(other),
        }
    }

    fn alloc_fid(&self) -> u32 {
        self.next_fid.fetch_add(1, Ordering::Relaxed)
    }

    fn clunk(&self, fid: u32) -> Result<()> {
        let buf = self.rpc(Fcall::Tclunk { fid })?;
        self.decode(&buf).map(drop)
    }

    /// `Some(afid)` when the server asked for a signature. `None` when it
    /// does not authenticate.
    fn authenticate(
        &self,
        user: &str,
        aname: &str,
        secret: impl Fn(&[u8]) -> Result<[u8; auth::PROOF]>,
    ) -> Result<Option<u32>> {
        const AFID: u32 = AUTH_FID;
        let buf = self.rpc(Fcall::Tauth {
            afid: AFID,
            uname: user,
            aname,
        })?;
        match self.decode(&buf) {
            Err(e) if e.as_str() == auth::NOT_REQUIRED => return Ok(None),
            Err(e) => return Err(e),
            Ok(Fcall::Rauth { .. }) => {}
            Ok(_) => return Err(Error::new(error::EIO)),
        }
        let buf = self.rpc(Fcall::Tread {
            fid: AFID,
            offset: 0,
            count: auth::CHALLENGE as u32,
        })?;
        let challenge = match self.decode(&buf)? {
            Fcall::Rread { data } if data.len() == auth::CHALLENGE => {
                let mut c = [0u8; auth::CHALLENGE];
                c.copy_from_slice(data);
                c
            }
            _ => return Err(Error::new(error::EIO)),
        };
        let proof = secret(&challenge)?;
        let buf = self.rpc(Fcall::Twrite {
            fid: AFID,
            offset: 0,
            data: &proof,
        })?;
        match self.decode(&buf)? {
            Fcall::Rwrite { count } if count as usize == proof.len() => Ok(Some(AFID)),
            _ => Err(Error::new(error::EIO)),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The reader kproc holds its own reference to the connection, so
        // closing this one does not unblock it. Killing its note makes the
        // blocked read return, the kproc drops its reference, and the pipe
        // finally hangs up.
        self.note.kill();
        self.mux.fail_all();
    }
}

fn file(c: &Chan) -> Result<&MntFile> {
    c.aux::<MntFile>().ok_or(Error::new(error::EBADUSEFD))
}

/// The `#M` device. One device serves every mount; `Chan::devno` is the mount id.
pub struct DevMnt {
    platform: Arc<dyn Platform>,
    next_id: AtomicU32,
}

impl DevMnt {
    pub fn new(platform: Arc<dyn Platform>) -> DevMnt {
        DevMnt {
            platform,
            next_id: AtomicU32::new(1),
        }
    }

    /// Starts the reader kproc for a new session on `fd`.
    fn start(&self, fd: Arc<Chan>, msize: u32) -> Result<Arc<Session>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mux = Arc::new(Mux::new());
        let note = Arc::new(Note::new());
        let session = Arc::new(Session {
            id,
            fd: fd.clone(),
            mux: mux.clone(),
            note: note.clone(),
            msize: Arc::new(AtomicU32::new(msize)),
            iounit: AtomicU32::new(msize.saturating_sub(IOHDRSZ)),
            write: Mutex::new(()),
            next_fid: AtomicU32::new(1),
        });
        let msize = session.msize.clone();
        self.platform.spawn_kproc(
            "mnt",
            Box::new(move || {
                sync::set_current_note(Some(note));
                loop {
                    let max = msize.load(Ordering::Acquire).max(IOHDRSZ);
                    match read_msg(&fd, max) {
                        Ok(buf) => match Msg::decode(&buf, max) {
                            Ok(msg) => {
                                mux.deliver(msg.tag, buf);
                            }
                            Err(_) => {
                                mux.fail_all();
                                break;
                            }
                        },
                        Err(_) => {
                            mux.fail_all();
                            break;
                        }
                    }
                }
                sync::set_current_note(None);
            }),
        )?;
        Ok(session)
    }

    /// `Tattach` on `session`, returning the root Chan.
    fn attach(
        self: &Arc<Self>,
        session: &Arc<Session>,
        afid: u32,
        user: &str,
        aname: &str,
    ) -> Result<Chan> {
        let fid = session.alloc_fid();
        let buf = session.rpc(Fcall::Tattach {
            fid,
            afid,
            uname: user,
            aname,
        })?;
        let qid = match session.decode(&buf)? {
            Fcall::Rattach { qid } => qid,
            _ => return Err(Error::new(error::EIO)),
        };
        let aux = Arc::new(MntFile {
            session: session.clone(),
            fid,
            live: AtomicBool::new(true),
            spec: String::from(aname),
            fresh: AtomicBool::new(false),
        });
        let dev: Arc<dyn Dev> = self.clone();
        let id = session.id;
        let mut c = Chan::new(dev, id, qid, alloc::format!("#M{id}"), Some(aux));
        if qid.is_dir() {
            c.qid.qtype |= QTDIR;
        }
        Ok(c)
    }

    /// Version and attach on `fd`, returning the attached root.
    ///
    /// `noauth` attaches with `NOFID`. Otherwise the client speaks `Tauth`
    /// and, when the server asks, signs the challenge with `secret`.
    pub fn mount(
        self: &Arc<Self>,
        fd: Arc<Chan>,
        aname: &str,
        user: &str,
        noauth: bool,
        secret: impl Fn(&[u8]) -> Result<[u8; auth::PROOF]>,
    ) -> Result<Chan> {
        let session = self.start(fd, MAX_MSIZE)?;
        let established = (|| {
            let buf = session.rpc(Fcall::Tversion {
                msize: MAX_MSIZE,
                version: VERSION,
            })?;
            match session.decode(&buf)? {
                Fcall::Rversion { msize: m, version } if version == VERSION => {
                    session.msize.store(m.max(IOHDRSZ), Ordering::Release);
                    session
                        .iounit
                        .store(m.saturating_sub(IOHDRSZ), Ordering::Release);
                }
                _ => return Err(Error::new(BAD_VERSION)),
            }
            let afid = if noauth {
                NOFID
            } else {
                session.authenticate(user, aname, &secret)?.unwrap_or(NOFID)
            };
            self.attach(&session, afid, user, aname)
        })();
        if established.is_err() {
            session.note.kill();
            session.mux.fail_all();
        }
        established
    }

    /// `sys->fauth`: versions the conversation on `fd` and sends `Tauth`.
    /// Returns an open Chan on the auth fid: reading it gives the server's
    /// challenge, and writing it sends the proof.
    ///
    /// The exchange up to `Rauth` runs on the caller's thread. A server that
    /// does not authenticate therefore leaves no session (and no reader)
    /// behind, and the fd can still be mounted with `afd = -1`.
    pub fn auth(self: &Arc<Self>, fd: Arc<Chan>, aname: &str, user: &str) -> Result<Chan> {
        let msize = sync_rpc(
            &fd,
            MAX_MSIZE,
            Fcall::Tversion {
                msize: MAX_MSIZE,
                version: VERSION,
            },
            |f| match f {
                Fcall::Rversion { msize, version } if version == VERSION => {
                    Ok(msize.clamp(IOHDRSZ, MAX_MSIZE))
                }
                _ => Err(Error::new(BAD_VERSION)),
            },
        )?;
        let aqid = sync_rpc(
            &fd,
            msize,
            Fcall::Tauth {
                afid: AUTH_FID,
                uname: user,
                aname,
            },
            |f| match f {
                Fcall::Rauth { aqid } => Ok(aqid),
                _ => Err(Error::new(error::EIO)),
            },
        )?;
        let session = self.start(fd, msize)?;
        let aux = Arc::new(MntFile {
            session: session.clone(),
            fid: AUTH_FID,
            live: AtomicBool::new(true),
            spec: String::from(aname),
            fresh: AtomicBool::new(false),
        });
        let dev: Arc<dyn Dev> = self.clone();
        let id = session.id;
        let mut c = Chan::new(dev, id, aqid, alloc::format!("#M{id}"), Some(aux));
        c.qid.qtype |= QTAUTH;
        c.mode = ORDWR;
        c.open = true;
        Ok(c)
    }

    /// `sys->mount` with an auth fd from [`DevMnt::auth`]: attaches on the
    /// session `afd` belongs to. `fd` must be that session's conversation.
    pub fn mount_auth(
        self: &Arc<Self>,
        fd: &Arc<Chan>,
        afd: &Chan,
        aname: &str,
        user: &str,
    ) -> Result<Chan> {
        let f = match afd.aux::<MntFile>() {
            Some(f) if afd.dev.dc() == 'M' && afd.qid.qtype & QTAUTH != 0 => f,
            _ => return Err(Error::new(error::EBADUSEFD)),
        };
        if !Arc::ptr_eq(&f.session.fd, fd) {
            return Err(Error::new(error::EBADUSEFD));
        }
        self.attach(&f.session, f.fid, user, aname)
    }
}

const BAD_VERSION: &str = "bad 9P version";

/// The fid `Tauth` uses. Session fids count up from 1 and do not reach it.
const AUTH_FID: u32 = NOFID - 1;

/// Sends one message on `fd` and reads the reply on this thread, before a
/// session's reader exists.
fn sync_rpc<T>(
    fd: &Chan,
    msize: u32,
    fcall: Fcall<'_>,
    reply: impl FnOnce(Fcall<'_>) -> Result<T>,
) -> Result<T> {
    let tag = if matches!(fcall, Fcall::Tversion { .. }) {
        NOTAG
    } else {
        0
    };
    let bytes = Msg { tag, fcall }
        .encode()
        .map_err(|_| Error::new(error::EIO))?;
    write_full(fd, &bytes)?;
    let buf = read_msg(fd, msize)?;
    let msg = Msg::decode(&buf, msize).map_err(|_| Error::new(error::EIO))?;
    if msg.tag != tag {
        return Err(Error::new(error::EIO));
    }
    match msg.fcall {
        Fcall::Rerror { ename } => Err(Error::from(String::from(ename))),
        other => reply(other),
    }
}

impl Dev for DevMnt {
    fn dc(&self) -> char {
        'M'
    }

    fn name(&self) -> &'static str {
        "mnt"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Err(Error::new("no mount"))
    }

    fn iounit(&self, c: &Chan) -> u32 {
        file(c).map(|f| f.session.iounit()).unwrap_or(0)
    }

    fn aname(&self, c: &Chan) -> String {
        Session::spec_of(c)
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let f = file(c)?;
        let newfid = f.session.alloc_fid();
        let buf = f.session.rpc(Fcall::Twalk {
            fid: f.fid,
            newfid,
            wnames: alloc::vec![name],
        });
        let buf = match buf {
            Ok(b) => b,
            Err(e) => {
                let _ = f.session.clunk(newfid);
                return Err(e);
            }
        };
        let qids = match f.session.decode(&buf) {
            Ok(Fcall::Rwalk { wqids }) => wqids,
            Ok(_) => return Err(Error::new(error::EIO)),
            Err(e) => {
                let _ = f.session.clunk(newfid);
                return Err(e);
            }
        };
        if qids.len() != 1 {
            let _ = f.session.clunk(newfid);
            return Err(Error::new(error::ENONEXIST));
        }
        Ok(Walked {
            qid: qids[0],
            aux: Some(Arc::new(MntFile {
                session: f.session.clone(),
                fid: newfid,
                live: AtomicBool::new(true),
                spec: String::new(),
                fresh: AtomicBool::new(false),
            })),
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let f = file(c)?;
        let buf = f.session.rpc(Fcall::Tstat { fid: f.fid })?;
        match f.session.decode(&buf)? {
            Fcall::Rstat { stat } => Dir::unpack(stat)
                .map(|(d, _)| d)
                .map_err(|_| Error::new(error::EIO)),
            _ => Err(Error::new(error::EIO)),
        }
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let f = file(c)?;
        if f.fresh.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let buf = f.session.rpc(Fcall::Topen {
            fid: f.fid,
            mode: omode,
        })?;
        match f.session.decode(&buf)? {
            Fcall::Ropen { .. } => Ok(()),
            _ => Err(Error::new(error::EIO)),
        }
    }

    fn create(&self, dir: &Chan, name: &str, perm: u32) -> Result<Walked> {
        let f = file(dir)?;
        let newfid = f.session.alloc_fid();
        let cloned = f.session.rpc(Fcall::Twalk {
            fid: f.fid,
            newfid,
            wnames: Vec::new(),
        })?;
        if let Err(e) = f.session.decode(&cloned) {
            let _ = f.session.clunk(newfid);
            return Err(e);
        }
        let mode = if perm & DMDIR != 0 {
            purg_abi::mode::OREAD
        } else {
            ORDWR
        };
        let buf = match f.session.rpc(Fcall::Tcreate {
            fid: newfid,
            name,
            perm,
            mode,
        }) {
            Ok(b) => b,
            Err(e) => {
                let _ = f.session.clunk(newfid);
                return Err(e);
            }
        };
        let qid = match f.session.decode(&buf) {
            Ok(Fcall::Rcreate { qid, .. }) => qid,
            Ok(_) => return Err(Error::new(error::EIO)),
            Err(e) => {
                let _ = f.session.clunk(newfid);
                return Err(e);
            }
        };
        Ok(Walked {
            qid,
            aux: Some(Arc::new(MntFile {
                session: f.session.clone(),
                fid: newfid,
                live: AtomicBool::new(true),
                spec: String::new(),
                fresh: AtomicBool::new(true),
            })),
        })
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let f = file(c)?;
        let count = (buf.len() as u32).min(f.session.iounit().max(1));
        let reply = f.session.rpc(Fcall::Tread {
            fid: f.fid,
            offset,
            count,
        })?;
        match f.session.decode(&reply)? {
            Fcall::Rread { data } => {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
            _ => Err(Error::new(error::EIO)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let f = file(c)?;
        let mut out = Vec::new();
        let mut offset = 0u64;
        let count = f.session.iounit().max(512);
        loop {
            let reply = f.session.rpc(Fcall::Tread {
                fid: f.fid,
                offset,
                count,
            })?;
            let data = match f.session.decode(&reply)? {
                Fcall::Rread { data } => data.to_vec(),
                _ => return Err(Error::new(error::EIO)),
            };
            if data.is_empty() {
                break;
            }
            // Only an empty read ends the directory: a server may return
            // fewer entries than fit (the 9P2000.u shim's replies shrink).
            offset += data.len() as u64;
            for d in dirs(&data) {
                out.push(d.map_err(|_| Error::new(error::EIO))?);
            }
        }
        Ok(out)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let f = file(c)?;
        let n = buf.len().min(f.session.iounit().max(1) as usize);
        let reply = f.session.rpc(Fcall::Twrite {
            fid: f.fid,
            offset,
            data: &buf[..n],
        })?;
        match f.session.decode(&reply)? {
            Fcall::Rwrite { count } => Ok(count as usize),
            _ => Err(Error::new(error::EIO)),
        }
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        let f = file(c)?;
        let buf = f.session.rpc(Fcall::Tremove { fid: f.fid })?;
        f.session.decode(&buf)?;
        f.live.store(false, Ordering::Release);
        Ok(())
    }

    fn wstat(&self, c: &Chan, d: &Dir) -> Result<()> {
        let f = file(c)?;
        let stat = d.pack().map_err(|_| Error::new(error::EIO))?;
        let buf = f.session.rpc(Fcall::Twstat {
            fid: f.fid,
            stat: &stat,
        })?;
        match f.session.decode(&buf)? {
            Fcall::Rwstat => Ok(()),
            _ => Err(Error::new(error::EIO)),
        }
    }
}
