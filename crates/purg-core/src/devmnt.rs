//! `#M`: a Styx client. `mount` speaks 9P2000 on a file descriptor and returns
//! a Chan for the attached root. A reader kproc demultiplexes replies by tag.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use purg_abi::error;
use purg_abi::mode::{self, ORDWR};
use purg_platform::sync::{Mutex, Note, Rendez};
use purg_platform::{Platform, sync};
use purg_styx::server::{IOHDRSZ, MAX_MSIZE};
use purg_styx::{
    DMDIR, Dir, Fcall, MAXWELEM, Msg, NOFID, NOTAG, QTAUTH, QTDIR, QTEXCL, Qid, VERSION, dirs,
};

use purg_auth::record::{self, Opener, Sealer};
use purg_auth::session;

use crate::auth::{self, ENCRYPTED, NO_SESSION, OFFER, SERVER_NONE, SERVER_PROOF_OFFSET, SIGNED};
use crate::chan::Chan;
use crate::dev::{Dev, Walked, WalkedNames};
use crate::error::{Error, Result};
use crate::mux::Mux;
use crate::rw::{read_msg, read_sealed, write_full, write_sealed};

struct Session {
    /// The mount id: `Chan::devno` of every file on this session.
    id: u32,
    fd: Arc<Chan>,
    mux: Arc<Mux>,
    note: Arc<Note>,
    msize: Arc<AtomicU32>,
    iounit: AtomicU32,
    /// Held across each write, so messages go out whole. Once a version 3
    /// handshake has agreed a key it holds the sealer of the client's
    /// direction.
    write: Mutex<Option<Sealer>>,
    /// The tag whose reply is the last one in the clear, and the opener
    /// the reader takes up after it.
    rekey: Arc<Mutex<Option<(u16, Opener)>>>,
    /// The exchange on an auth fd from [`DevMnt::auth`].
    kx: Mutex<Option<Kx>>,
    /// The keys the conversation switched to, for the server's proof.
    keys: Mutex<Option<session::Keys>>,
    /// The server's proof, as first read from it. A guest that reads it
    /// from the auth fd (to check the host it is for) and the mount's own
    /// check (`MSRVAUTH`) then see the same bytes.
    server: Mutex<Option<Vec<u8>>>,
    /// Conversations a version 3 proof switched to records, shared by
    /// every session of the device.
    sealed: Arc<Sealed>,
    platform: Arc<dyn Platform>,
    next_fid: AtomicU32,
}

/// What a client keeps between reading the server's offer from an auth fd
/// and writing its proof.
struct Kx {
    user: String,
    aname: String,
    /// This client's ephemeral key for the offer, made by the first read
    /// of it. Every later read gives the same bytes, so a read from
    /// another thread cannot change the key under a proof being written.
    client: Option<session::Client>,
}

/// The conversations (by their fd's Chan) that went to records. One of
/// them cannot be versioned or authenticated again in the clear: the
/// server opens everything on it as records, so a new `Tversion` would
/// only come back as a hangup.
#[derive(Default)]
struct Sealed(Mutex<Vec<Weak<Chan>>>);

impl Sealed {
    fn add(&self, fd: &Arc<Chan>) {
        let mut v = self.0.lock();
        v.retain(|w| w.strong_count() > 0);
        v.push(Arc::downgrade(fd));
    }

    /// Fails with [`ENCRYPTED`] when `fd` went to records.
    fn check(&self, fd: &Arc<Chan>) -> Result<()> {
        let mut v = self.0.lock();
        v.retain(|w| w.strong_count() > 0);
        if v.iter().any(|w| core::ptr::eq(w.as_ptr(), Arc::as_ptr(fd))) {
            return Err(Error::new(ENCRYPTED));
        }
        Ok(())
    }
}

/// Set by a session's reader kproc as it exits.
#[derive(Default)]
struct Reader {
    done: AtomicBool,
    gone: Rendez,
}

impl Reader {
    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.gone.wakeup();
    }

    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// How long a new session waits for the reader of a dead one on the same
/// fd to go. Its note was killed when the session went, so this is only
/// a bound.
const READER_GONE_NS: u64 = 5_000_000_000;

/// One session of the device: the conversation it reads, the session, and
/// its reader.
struct Entry {
    fd: Weak<Chan>,
    session: Weak<Session>,
    reader: Arc<Reader>,
}

impl Entry {
    fn over(&self) -> bool {
        self.session.strong_count() == 0 && self.reader.finished()
    }
}

/// The sessions of the device, by the conversation they read. Two
/// sessions on one fd would race each other's reader for its replies, so
/// a fd whose session is still alive cannot be versioned again, and one
/// whose session has gone is versioned only once its reader has stopped.
#[derive(Default)]
struct Live(Mutex<Vec<Entry>>);

impl Live {
    fn add(&self, s: &Arc<Session>, reader: Arc<Reader>) {
        let mut v = self.0.lock();
        v.retain(|e| !e.over());
        v.push(Entry {
            fd: Arc::downgrade(&s.fd),
            session: Arc::downgrade(s),
            reader,
        });
    }

    /// Fails with `EINUSE` while a session on `fd` lives: an auth fd from
    /// `fauth`, or a mount, still holds it. When the session has gone but
    /// its reader is still on its way out, waits for it.
    fn check(&self, fd: &Arc<Chan>) -> Result<()> {
        let mut v = self.0.lock();
        v.retain(|e| !e.over());
        let mut dying = Vec::new();
        for e in v
            .iter()
            .filter(|e| core::ptr::eq(e.fd.as_ptr(), Arc::as_ptr(fd)))
        {
            if e.session.strong_count() > 0 {
                return Err(Error::new(error::EINUSE));
            }
            dying.push(e.reader.clone());
        }
        drop(v);
        for r in dying {
            if !r.gone.tsleep(|| r.finished(), READER_GONE_NS) && !r.finished() {
                return Err(Error::new(error::EINUSE));
            }
        }
        Ok(())
    }
}

/// Checks a server's proof (`purg_auth::server`) for the session's keys:
/// what `sys->mount` passes with `MSRVAUTH`.
pub type ServerCheck<'a> = &'a dyn Fn(&session::Keys, &[u8]) -> Result<()>;

struct MntFile {
    session: Arc<Session>,
    fid: u32,
    live: AtomicBool,
    spec: String,
    /// Set when `Tcreate` already opened the fid, so the following `open`
    /// must not send `Topen`.
    fresh: AtomicBool,
    /// The fid was opened in a way whose clunk a later request may depend
    /// on, so dropping it waits for `Rclunk` ([`MntFile::settles`]).
    settle: AtomicBool,
}

impl MntFile {
    fn new(session: &Arc<Session>, fid: u32, spec: &str) -> MntFile {
        MntFile {
            session: session.clone(),
            fid,
            live: AtomicBool::new(true),
            spec: String::from(spec),
            fresh: AtomicBool::new(false),
            settle: AtomicBool::new(false),
        }
    }

    /// Whether the clunk of a fid opened with `omode` on a file with
    /// `qid` must be waited for. A server may serve requests concurrently
    /// (`export` does), so a request sent after a `Tclunk` that is not
    /// waited for can be served first. That matters only when the clunk
    /// changes what a later request sees: it removes the file
    /// (`ORCLOSE`), ends an exclusive use (`QTEXCL`), or ends a write a
    /// server may finish on close. Everything else (the fids a walk
    /// leaves, a stat's, a read-only open's) is clunked without waiting.
    fn settles(omode: u8, qid: &Qid) -> bool {
        let access = omode & mode::ACCESS;
        (access != mode::OREAD && access != mode::OEXEC)
            || omode & (mode::ORCLOSE | mode::OTRUNC) != 0
            || qid.qtype & QTEXCL != 0
    }
}

impl Drop for MntFile {
    fn drop(&mut self) {
        if self.live.swap(false, Ordering::AcqRel) {
            let wait = self.settle.load(Ordering::Acquire);
            let _ = self.session.clunk(self.fid, wait);
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
        self.send(&bytes)?;
        match self.mux.wait(tag) {
            Ok(buf) => Ok(buf),
            Err(e) if e.as_str() == error::EINTR => {
                self.flush_tag(tag);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Writes one whole message, sealed once the session has a key.
    fn send(&self, bytes: &[u8]) -> Result<()> {
        let mut g = self.write.lock();
        write_sealed(&self.fd, g.as_mut(), bytes)
    }

    /// Whether the conversation is encrypted.
    fn sealed(&self) -> bool {
        self.write.lock().is_some()
    }

    /// Sends the version 3 proof `fcall`, the last message in the clear,
    /// and switches the conversation to records under `keys`: everything
    /// written after it is sealed, and everything read after its reply is
    /// opened. A conversation that is already sealed keeps its keys.
    ///
    /// No `Tflush` can be sent for it: a prog killed while it waits takes
    /// the whole session down instead, so both ends always agree where
    /// the records start.
    fn rpc_switch(&self, fcall: Fcall<'_>, keys: &session::Keys) -> Result<Vec<u8>> {
        let tag = self.mux.begin()?;
        let bytes = Msg { tag, fcall }
            .encode()
            .map_err(|_| Error::new(error::EIO))?;
        let (sealer, opener) = keys.client();
        let mut g = self.write.lock();
        if g.is_some() {
            drop(g);
            return self.wait_or_fail(tag, self.send(&bytes));
        }
        // The reader must know the switch before the reply can arrive,
        // and nothing else may be written between the proof and the
        // sealer: both happen under the write lock.
        *self.rekey.lock() = Some((tag, opener));
        let sent = write_full(&self.fd, &bytes);
        if sent.is_ok() {
            *g = Some(sealer);
            *self.keys.lock() = Some(keys.clone());
            self.sealed.add(&self.fd);
        }
        drop(g);
        self.wait_or_fail(tag, sent)
    }

    /// Reads the server's proof from the auth fid and checks it with
    /// `check` against this session's keys. A conversation that is not
    /// encrypted has no proof to read.
    fn check_server(&self, afid: u32, check: ServerCheck<'_>) -> Result<()> {
        let Some(keys) = self.keys.lock().clone() else {
            return Err(Error::new(SERVER_NONE));
        };
        let proof = self.server_proof(afid)?;
        check(&keys, &proof)
    }

    /// The server's proof, read from the auth fid on the first call and
    /// the same bytes on every later one, so what a guest checked is what
    /// the mount checks. Only an encrypted conversation has one: the
    /// callers look for its keys first.
    fn server_proof(&self, afid: u32) -> Result<Vec<u8>> {
        if let Some(p) = self.server.lock().clone() {
            return Ok(p);
        }
        let reply = self.rpc(Fcall::Tread {
            fid: afid,
            offset: SERVER_PROOF_OFFSET,
            count: purg_auth::proof::MAX_LEN as u32,
        })?;
        let proof = match self.decode(&reply)? {
            Fcall::Rread { data } => data.to_vec(),
            _ => return Err(Error::new(error::EIO)),
        };
        // Two first reads at once: the first one stored wins.
        Ok(self.server.lock().get_or_insert(proof).clone())
    }

    fn wait_or_fail(&self, tag: u16, sent: Result<()>) -> Result<Vec<u8>> {
        let got = sent.and_then(|()| self.mux.wait(tag));
        if got.is_err() {
            self.note.kill();
            self.mux.fail_all();
        }
        got
    }

    /// A read of the auth fd that wants a version 3 exchange: fetches the
    /// server's offer and returns it followed by a fresh ephemeral key of
    /// this client's ([`SIGNED`] bytes). A server that offers only a
    /// challenge gives just that, and the caller decides.
    fn read_offer(&self, buf: &mut [u8]) -> Result<usize> {
        if let Some(c) = self.kx.lock().as_ref().and_then(|k| k.client.as_ref()) {
            buf[..SIGNED].copy_from_slice(&c.signed);
            return Ok(SIGNED);
        }
        let reply = self.rpc(Fcall::Tread {
            fid: AUTH_FID,
            offset: 0,
            count: OFFER as u32,
        })?;
        let data = match self.decode(&reply)? {
            Fcall::Rread { data } => data,
            _ => return Err(Error::new(error::EIO)),
        };
        let offer: &[u8; OFFER] = match data.try_into() {
            Ok(o) if !self.sealed() => o,
            _ => {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                return Ok(n);
            }
        };
        let client = self.client(offer)?;
        // Two first reads at once: the one that stores its key wins, and
        // both return that key.
        let mut kx = self.kx.lock();
        let signed = match kx.as_mut() {
            Some(k) => k.client.get_or_insert(client).signed,
            None => client.signed,
        };
        drop(kx);
        buf[..SIGNED].copy_from_slice(&signed);
        Ok(SIGNED)
    }

    /// The session keys for `proof`, written to the auth fd, when it is a
    /// version 3 proof over the offer [`Session::read_offer`] gave.
    fn proof_keys(&self, proof: &[u8]) -> Option<session::Keys> {
        let kx = self.kx.lock();
        let kx = kx.as_ref()?;
        kx.client.as_ref()?.keys(&kx.user, &kx.aname, proof)
    }

    /// This client's answer to the server's `offer`: a fresh ephemeral key.
    fn client(&self, offer: &[u8; OFFER]) -> Result<session::Client> {
        let mut random = [0u8; 32];
        self.platform.entropy(&mut random);
        session::Client::new(offer, random).ok_or(Error::new(auth::FAILED))
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
        let sent = self.send(&bytes);
        if sent.is_err() {
            // No `Rflush` will come for a request that was never sent.
            self.mux.forget(ftag);
            self.mux.forget(tag);
            return;
        }
        // The flushed tag is taken until the `Rflush` arrives. A killed
        // prog does not wait for it: the reader frees both tags then.
        match self.mux.wait(ftag) {
            Ok(_) => self.mux.forget(tag),
            Err(e) if e.as_str() == error::EINTR => self.mux.abandon(ftag, tag),
            Err(_) => {}
        }
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

    /// `Tclunk`. A server clunks the fid whatever it answers, and an error
    /// in `Rclunk` is ignored, as Inferno's `cclose` ignores it. So unless
    /// `wait` is set the request is sent and not waited for: the reader
    /// drops the reply when it comes.
    ///
    /// With `wait`, a prog that is killed while it waits does not flush
    /// the request, which could leave the fid open on the server: the
    /// request stands, the prog goes on (usually it is exiting), and the
    /// reader drops the reply. A server that never answers keeps the tag
    /// until the connection hangs up.
    fn clunk(&self, fid: u32, wait: bool) -> Result<()> {
        let tag = self.mux.begin()?;
        let bytes = Msg {
            tag,
            fcall: Fcall::Tclunk { fid },
        }
        .encode()
        .map_err(|_| Error::new(error::EIO))?;
        if let Err(e) = self.send(&bytes) {
            self.mux.forget(tag);
            return Err(e);
        }
        if !wait {
            self.mux.discard(tag);
            return Ok(());
        }
        match self.mux.wait(tag) {
            Ok(buf) => self.decode(&buf).map(drop),
            Err(e) if e.as_str() == error::EINTR => {
                self.mux.discard(tag);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// `Some(afid)` when the server asked for a proof. `None` when it does
    /// not authenticate.
    ///
    /// The exchange is version 3 (`docs/auth.md`): the server offers its
    /// challenge and an ephemeral key, `secret` signs that offer and this
    /// client's ephemeral key, and the conversation is encrypted from the
    /// proof on. A server that offers no key is refused, so nothing on
    /// the way can strip the offer and keep the conversation in the clear.
    fn authenticate(
        &self,
        user: &str,
        aname: &str,
        secret: impl Fn(&[u8]) -> Result<Vec<u8>>,
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
            count: OFFER as u32,
        })?;
        let offer: [u8; OFFER] = match self.decode(&buf)? {
            Fcall::Rread { data } if data.len() == OFFER => {
                data.try_into().map_err(|_| Error::new(error::EIO))?
            }
            Fcall::Rread { data } if data.len() == auth::CHALLENGE => {
                return Err(Error::new(NO_SESSION));
            }
            _ => return Err(Error::new(error::EIO)),
        };
        let client = self.client(&offer)?;
        let proof = secret(&client.signed)?;
        let keys = client
            .keys(user, aname, &proof)
            .ok_or(Error::new(auth::FAILED))?;
        let buf = self.rpc_switch(
            Fcall::Twrite {
                fid: AFID,
                offset: 0,
                data: &proof,
            },
            &keys,
        )?;
        match self.decode(&buf)? {
            Fcall::Rwrite { count } if count as usize == proof.len() => Ok(Some(AFID)),
            _ => Err(Error::new(error::EIO)),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The replies still due, to clunks nobody waited for, are read
        // first: a server is not hung up on with replies it has yet to
        // write. A prog killed meanwhile does not wait.
        let _ = self.mux.settle();
        // The reader kproc holds its own reference to the connection, so
        // closing this one does not unblock it. Killing its note makes the
        // blocked read return, the kproc drops its reference, and the pipe
        // finally hangs up.
        self.note.kill();
        self.mux.fail_all();
    }
}

/// Stamps a Dir from the server with this mount's device type and number,
/// whatever the server put there, as Inferno's mntdirfix does.
fn fix(c: &Chan, mut d: Dir) -> Dir {
    d.dtype = u16::from(b'M');
    d.dev = c.devno;
    d
}

fn file(c: &Chan) -> Result<&MntFile> {
    c.aux::<MntFile>().ok_or(Error::new(error::EBADUSEFD))
}

/// The `#M` device. One device serves every mount; `Chan::devno` is the mount id.
pub struct DevMnt {
    platform: Arc<dyn Platform>,
    next_id: AtomicU32,
    sealed: Arc<Sealed>,
    live: Live,
}

impl DevMnt {
    pub fn new(platform: Arc<dyn Platform>) -> DevMnt {
        DevMnt {
            platform,
            next_id: AtomicU32::new(1),
            sealed: Arc::new(Sealed::default()),
            live: Live::default(),
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
            write: Mutex::new(None),
            rekey: Arc::new(Mutex::new(None)),
            kx: Mutex::new(None),
            keys: Mutex::new(None),
            server: Mutex::new(None),
            sealed: self.sealed.clone(),
            platform: self.platform.clone(),
            next_fid: AtomicU32::new(1),
        });
        let msize = session.msize.clone();
        let rekey = session.rekey.clone();
        let reader = Arc::new(Reader::default());
        let done = reader.clone();
        self.platform.spawn_kproc(
            "mnt",
            Box::new(move || {
                sync::set_current_note(Some(note));
                // The server's direction, once the version 3 proof's reply
                // has been read.
                let mut opener: Option<Opener> = None;
                loop {
                    let max = msize.load(Ordering::Acquire).max(IOHDRSZ);
                    let got = match opener.as_mut() {
                        Some(o) => read_sealed(&fd, o, max),
                        None => read_msg(&fd, max),
                    };
                    let tag = match &got {
                        Ok(buf) => Msg::decode(buf, max).map(|m| m.tag).ok(),
                        Err(_) => None,
                    };
                    match (got, tag) {
                        (Ok(buf), Some(tag)) => {
                            if opener.is_none() {
                                let mut k = rekey.lock();
                                if k.as_ref().is_some_and(|(t, _)| *t == tag) {
                                    opener = k.take().map(|(_, o)| o);
                                }
                            }
                            mux.deliver(tag, buf);
                        }
                        (Err(e), _) => {
                            match record::error(e.as_str()) {
                                Some(why) => mux.fail_because(why),
                                None => mux.fail_all(),
                            }
                            break;
                        }
                        (Ok(_), None) => {
                            mux.fail_all();
                            break;
                        }
                    }
                }
                sync::set_current_note(None);
                drop(fd);
                done.finish();
            }),
        )?;
        self.live.add(&session, reader);
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
        let aux = Arc::new(MntFile::new(session, fid, aname));
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
    /// and, when the server asks, signs the challenge with `secret`. With
    /// `server`, the server must prove itself after that, and `server`
    /// checks its proof; a server that asks for no proof cannot.
    ///
    /// A conversation that went to records earlier fails with
    /// [`ENCRYPTED`], and one another session still reads (an open auth fd
    /// from [`DevMnt::auth`], or a mount) with `EINUSE`.
    pub fn mount(
        self: &Arc<Self>,
        fd: Arc<Chan>,
        aname: &str,
        user: &str,
        noauth: bool,
        secret: impl Fn(&[u8]) -> Result<Vec<u8>>,
        server: Option<ServerCheck<'_>>,
    ) -> Result<Chan> {
        self.sealed.check(&fd)?;
        self.live.check(&fd)?;
        // The version exchange runs on the caller's thread, with `NOTAG`
        // as version(5) requires: tags belong to the session it starts.
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
        let session = self.start(fd, msize)?;
        let established = (|| {
            let afid = if noauth {
                NOFID
            } else {
                session.authenticate(user, aname, &secret)?.unwrap_or(NOFID)
            };
            if let Some(check) = server {
                if afid == NOFID {
                    return Err(Error::new(SERVER_NONE));
                }
                session.check_server(afid, check)?;
            }
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
    /// A read of at least [`SIGNED`] bytes at offset 0 fetches the server's
    /// offer and returns it with a fresh ephemeral key of this kernel's
    /// appended: the bytes a version 3 proof signs. Writing that proof
    /// then switches the conversation to records, and the ephemeral
    /// secret never leaves the kernel. A shorter read gives the bare
    /// challenge, for a version 1 or 2 proof and a conversation in the
    /// clear.
    ///
    /// The exchange up to `Rauth` runs on the caller's thread. A server that
    /// does not authenticate therefore leaves no session (and no reader)
    /// behind, and the fd can still be mounted with `afd = -1`.
    ///
    /// A conversation that went to records earlier fails with
    /// [`ENCRYPTED`], and one another session still reads with `EINUSE`.
    ///
    /// Once the conversation is encrypted, a read at
    /// [`SERVER_PROOF_OFFSET`] or later gives the server's proof, read
    /// from the server once: a mount with `MSRVAUTH` checks the same
    /// bytes (`docs/auth.md`, "Servers per host").
    pub fn auth(self: &Arc<Self>, fd: Arc<Chan>, aname: &str, user: &str) -> Result<Chan> {
        self.sealed.check(&fd)?;
        self.live.check(&fd)?;
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
        *session.kx.lock() = Some(Kx {
            user: String::from(user),
            aname: String::from(aname),
            client: None,
        });
        let aux = MntFile::new(&session, AUTH_FID, aname);
        aux.settle.store(true, Ordering::Release);
        let aux = Arc::new(aux);
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
    /// With `server`, the server's proof is read from `afd` and checked
    /// first.
    pub fn mount_auth(
        self: &Arc<Self>,
        fd: &Arc<Chan>,
        afd: &Chan,
        aname: &str,
        user: &str,
        server: Option<ServerCheck<'_>>,
    ) -> Result<Chan> {
        let f = match afd.aux::<MntFile>() {
            Some(f) if afd.dev.dc() == 'M' && afd.qid.qtype & QTAUTH != 0 => f,
            _ => return Err(Error::new(error::EBADUSEFD)),
        };
        if !Arc::ptr_eq(&f.session.fd, fd) {
            return Err(Error::new(error::EBADUSEFD));
        }
        if let Some(check) = server {
            f.session.check_server(f.fid, check)?;
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

    /// The attached root keeps the name `attach` gave it, `#M` and the
    /// mount id, until it is resolved by name: the mount table then gives
    /// it the path (Inferno's test of `#M`).
    fn mount_source(&self, c: &Chan) -> Option<String> {
        let f = file(c).ok()?;
        let root = alloc::format!("#M{}", f.session.id);
        (c.name == root).then(|| f.session.fd.name.clone())
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let w = self.walk_names(c, &[name])?;
        match (w.qids.first(), w.aux) {
            (Some(q), Some(aux)) => Ok(Walked {
                qid: *q,
                aux: Some(aux),
            }),
            _ => Err(Error::new(error::ENONEXIST)),
        }
    }

    fn max_walk(&self) -> usize {
        MAXWELEM
    }

    /// One `Twalk` for all of `names`. A walk that stops short gives the
    /// qids the server walked and leaves no fid to clunk.
    fn walk_names(&self, c: &Chan, names: &[&str]) -> Result<WalkedNames> {
        let f = file(c)?;
        if names.is_empty() || names.len() > MAXWELEM {
            return Err(Error::new(error::EIO));
        }
        let newfid = f.session.alloc_fid();
        let buf = f.session.rpc(Fcall::Twalk {
            fid: f.fid,
            newfid,
            wnames: names.to_vec(),
        });
        let buf = match buf {
            Ok(b) => b,
            Err(e) => {
                // Interrupted: the walk may have made `newfid` all the same.
                let _ = f.session.clunk(newfid, false);
                return Err(e);
            }
        };
        // A walk that fails, or stops short, does not create `newfid`:
        // there is nothing to clunk.
        let qids = match f.session.decode(&buf)? {
            Fcall::Rwalk { wqids } => wqids,
            _ => return Err(Error::new(error::EIO)),
        };
        if qids.is_empty() {
            return Err(Error::new(error::ENONEXIST));
        }
        if qids.len() > names.len() {
            let _ = f.session.clunk(newfid, false);
            return Err(Error::new(error::EIO));
        }
        let aux: crate::chan::Aux = if qids.len() == names.len() {
            Some(Arc::new(MntFile::new(&f.session, newfid, "")))
        } else {
            None
        };
        Ok(WalkedNames { qids, aux })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let f = file(c)?;
        let buf = f.session.rpc(Fcall::Tstat { fid: f.fid })?;
        match f.session.decode(&buf)? {
            Fcall::Rstat { stat } => Dir::unpack(stat)
                .map(|(d, _)| fix(c, d))
                .map_err(|_| Error::new(error::EIO)),
            _ => Err(Error::new(error::EIO)),
        }
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let f = file(c)?;
        if f.fresh.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        // Set before `Topen` goes out: a fid the server opened is clunked
        // as opened, even when this prog is killed waiting for `Ropen`.
        if MntFile::settles(omode, &c.qid) {
            f.settle.store(true, Ordering::Release);
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
        f.session.decode(&cloned)?;
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
                // Interrupted: the create may have opened `newfid`.
                let _ = f.session.clunk(newfid, true);
                return Err(e);
            }
        };
        let qid = match f.session.decode(&buf) {
            Ok(Fcall::Rcreate { qid, .. }) => qid,
            Ok(_) => return Err(Error::new(error::EIO)),
            Err(e) => {
                let _ = f.session.clunk(newfid, false);
                return Err(e);
            }
        };
        let aux = MntFile::new(&f.session, newfid, "");
        aux.fresh.store(true, Ordering::Release);
        aux.settle.store(true, Ordering::Release);
        Ok(Walked {
            qid,
            aux: Some(Arc::new(aux)),
        })
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let f = file(c)?;
        if f.fid == AUTH_FID && offset == 0 && buf.len() >= SIGNED {
            return f.session.read_offer(buf);
        }
        if f.fid == AUTH_FID && offset >= SERVER_PROOF_OFFSET && f.session.keys.lock().is_some() {
            let proof = f.session.server_proof(f.fid)?;
            let start = usize::try_from(offset - SERVER_PROOF_OFFSET).unwrap_or(usize::MAX);
            let data = proof.get(start..).unwrap_or(&[]);
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            return Ok(n);
        }
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
                out.push(fix(c, d.map_err(|_| Error::new(error::EIO))?));
            }
        }
        Ok(out)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let f = file(c)?;
        let n = buf.len().min(f.session.iounit().max(1) as usize);
        let fcall = Fcall::Twrite {
            fid: f.fid,
            offset,
            data: &buf[..n],
        };
        let keys = if f.fid == AUTH_FID {
            f.session.proof_keys(&buf[..n])
        } else {
            None
        };
        let reply = match keys {
            Some(k) => f.session.rpc_switch(fcall, &k)?,
            None => f.session.rpc(fcall)?,
        };
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
