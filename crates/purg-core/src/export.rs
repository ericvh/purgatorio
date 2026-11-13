//! `export`: serve a namespace over a file descriptor as a Styx server.
//!
//! Requests are served concurrently, as Inferno's exportfs does, so one
//! that blocks (a read of a pipe, of a file2chan file, of a slow device)
//! does not hold up the others on the connection:
//!
//! - The caller's thread reads requests. It answers `Tversion` and
//!   `Tflush` itself and hands everything else to a pool of worker kprocs
//!   ([`WorkQ`]), which grows when a request arrives and no worker is free.
//! - A worker runs one request at a time under a [`Note`] of the request's
//!   own. `Tflush` kills that note, which interrupts the worker if it is
//!   asleep in a device. The flushed request still runs to its end (it
//!   may be a `Tclunk`), and `Rflush` is sent after its reply, or in place
//!   of it when the request was interrupted. So no reply to a flushed tag
//!   can follow the `Rflush`, as flush(5) requires.
//! - Replies go through an [`OutQ`]: one thread at a time writes, whole
//!   messages, with no lock held across the write.
//! - The fid table is a map behind a lock, taken only to look a fid up or
//!   to change the table. A request works on its own reference to the fid,
//!   so a read and a write on one fid can run together, and a fid that is
//!   clunked under a blocked request is closed when that request ends.
//! - When the client hangs up, or the exporting prog is killed, every
//!   request's note is killed and `serve` returns once the workers have
//!   gone.
//! - After a version 3 proof (`docs/auth.md`) the conversation is records.
//!   The reader opens requests from the message after the proof on; the
//!   proof's reply is queued with the sealer behind it, and the thread
//!   draining the queue seals every reply after that, so the records go
//!   out in the order of their sequence numbers whichever worker answered.
//! - Once a version 3 proof is accepted, a read of the auth fid at offset
//!   64 gives this server's own proof (`purg_auth::server`), signed with
//!   the key a mount by the exporting user would prove with. It is made
//!   on the first such read, so a client that does not ask costs nothing.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use purg_abi::{error, mode};
use purg_auth::record::{Opener, Sealer};
use purg_platform::Platform;
use purg_platform::sync::{self, Mutex, Note, Rendez};
use purg_styx::server::{StyxFs, dispatch};
use purg_styx::{Dir, Fcall, Msg, NOFID, QTAUTH, Qid};

use crate::block::park;
use crate::chan::Chan;
use crate::error::{Error, Result as KResult};
use crate::names::cleanname;
use crate::pgrp::{Pgrp, absolute};
use crate::rw::{Pool, read_msg_in, read_sealed_in, write_full};
use crate::sysfile::{self, open};
use crate::workq::{OutQ, Put, WorkQ};

/// The most worker kprocs one export runs. With all of them blocked,
/// further requests wait (a `Tflush` still gets through).
pub const MAX_WORKERS: usize = 32;
/// Idle workers kept for the next request; others exit.
const KEEP_IDLE: usize = 2;

/// What a fid refers to. Never changed once it is in the table: `Topen`
/// and `Tcreate` put a new one in its place.
struct Fid {
    path: String,
    chan: Chan,
    /// The file's qid path on this connection.
    uqid: Uqid,
}

/// Qid paths for the files of an export.
///
/// A qid path is unique within one device, and every device's root is 0,
/// but a client sees the export as one tree: it tells files apart by qid
/// path alone, so two files with one path are one file to its mount table
/// (a bind on `/dev` would show on `/prog`). As Inferno's exportfs does,
/// each file a fid refers to, identified by device type, device number
/// and path, is given a path no other such file has: its own when that is
/// free, otherwise a made-up one. The path is the file's for as long as
/// some fid refers to it, which is as long as a client can have it in a
/// mount table.
///
/// Only the qids of fids are mapped (`Rattach`, `Rwalk`, `Ropen`,
/// `Rcreate`, `Rstat`). The entries of a directory read carry the qids
/// their devices gave, as in Inferno.
#[derive(Default)]
struct Uqids {
    /// (device type, device number, qid path) to the path given out, and
    /// how many [`Uqid`]s hold it.
    files: BTreeMap<(char, u32, u64), (u64, usize)>,
    /// The paths given out.
    taken: alloc::collections::BTreeSet<u64>,
    /// Counts the made-up paths.
    made: u64,
}

/// Made-up paths have the top bit set, where devices seldom put anything.
const UQID_MADE: u64 = 1 << 63;

/// One reference to a file's qid path on the connection.
struct Uqid {
    table: Arc<Mutex<Uqids>>,
    file: (char, u32, u64),
    path: u64,
}

impl Uqid {
    fn new(table: &Arc<Mutex<Uqids>>, c: &Chan) -> Uqid {
        let file = c.key();
        let mut t = table.lock();
        let path = match t.files.get_mut(&file) {
            Some((path, refs)) => {
                *refs += 1;
                *path
            }
            None => {
                let mut path = file.2;
                while t.taken.contains(&path) {
                    t.made += 1;
                    path = UQID_MADE | t.made;
                }
                t.taken.insert(path);
                t.files.insert(file, (path, 1));
                path
            }
        };
        drop(t);
        Uqid {
            table: table.clone(),
            file,
            path,
        }
    }

    /// `q` as the client sees it.
    fn qid(&self, q: Qid) -> Qid {
        Qid {
            path: self.path,
            ..q
        }
    }
}

impl Drop for Uqid {
    fn drop(&mut self) {
        let mut t = self.table.lock();
        if let Some((_, refs)) = t.files.get_mut(&self.file) {
            *refs -= 1;
            if *refs == 0 {
                t.files.remove(&self.file);
                t.taken.remove(&self.path);
            }
        }
    }
}

/// A request a worker has not answered yet.
struct Flight {
    /// The note the request runs under.
    note: Arc<Note>,
    /// Tags of the `Tflush`es waiting for it, oldest first.
    flushes: Vec<u16>,
    /// A `Tversion` aborted it: nothing is sent for it.
    aborted: bool,
}

/// One request, as read.
struct Req {
    tag: u16,
    buf: Vec<u8>,
}

/// One exported connection, shared by the reader and the workers.
struct Conn {
    fs: Export,
    fd: Arc<Chan>,
    platform: Arc<dyn Platform>,
    noauth: bool,
    /// Refuse what would leave the conversation in the clear.
    encrypt: bool,
    /// The exporting user, whose key signs this server's proof.
    user: String,
    /// The authinfo that signs this server's proof instead (`export -k`).
    keyfile: Option<String>,
    /// The exporting prog: workers serve files as it would.
    pid: u32,
    msize: AtomicU32,
    afids: Mutex<BTreeMap<u32, Afid>>,
    /// Requests handed to the workers and not yet answered, by tag.
    flights: Mutex<BTreeMap<u16, Flight>>,
    /// Woken when `flights` empties.
    quiet: Rendez,
    work: WorkQ<Req>,
    out: OutQ<Out>,
    /// Buffers for the requests the reader reads; each is handed back
    /// once its request is served ([`crate::rw::Pool`]).
    bufs: Pool,
    /// The tag of the version 3 proof the reader switched on, and the
    /// sealer the replies take up once its reply is queued.
    switch: Mutex<Option<(u16, Sealer)>>,
    /// The keys the conversation switched to, for this server's proof.
    keys: Mutex<Option<purg_auth::session::Keys>>,
    /// This server's proof for those keys, made when first asked for.
    server: Mutex<Option<Vec<u8>>>,
    /// The sealer of this server's direction. Only the thread draining
    /// [`Conn::out`] touches it, so records go out in the order of their
    /// sequence numbers.
    sealer: Mutex<Option<Sealer>>,
    /// The note a worker writes replies under, so a write blocked on a
    /// client that does not read can be interrupted at the end.
    wnote: Arc<Note>,
    /// Nothing more can be written.
    dead: AtomicBool,
}

/// Serves `root` (a path in `pgrp`) as 9P2000 on `fd` until the client hangs up.
///
/// With `rdonly`, nothing in the tree can be changed through the connection.
///
/// When `noauth` is false, `Tattach` is accepted only after a proof over a
/// challenge from `platform`, by a key or a signer `pgrp` (the exporter's
/// namespace) trusts (`docs/auth.md`). An export that trusts nothing then
/// lets nobody in: `Tauth` and `Tattach` fail with
/// [`NO_TRUST`](crate::auth::NO_TRUST).
///
/// Requests are served by worker kprocs, started through `platform` as
/// they are needed. They have all exited when this returns.
pub fn serve(
    pgrp: Arc<Pgrp>,
    root: &str,
    fd: &Arc<Chan>,
    platform: &Arc<dyn Platform>,
    noauth: bool,
    rdonly: bool,
) -> KResult<()> {
    let opts = Opts {
        noauth,
        rdonly,
        ..Opts::default()
    };
    serve_with(pgrp, root, fd, platform, &opts)
}

/// How an export serves, beside its tree.
#[derive(Default, Clone)]
pub struct Opts {
    /// Ask no proof, even when the exporter's namespace trusts a key.
    pub noauth: bool,
    /// Refuse every change to the tree.
    pub rdonly: bool,
    /// Refuse version 1 and 2 proofs, and any attach that is not on an
    /// encrypted conversation (`purg_abi::export::ENCRYPT`).
    pub encrypt: bool,
    /// The exporting user: its key, found as a mount by it would find
    /// one, signs this server's proof. Empty finds only `#¤` and the test
    /// key.
    pub user: String,
    /// An authinfo (a path in the exporter's namespace) whose key signs
    /// this server's proof instead of the user's (`export -k`).
    pub keyfile: Option<String>,
}

/// [`serve`] with every option.
pub fn serve_with(
    pgrp: Arc<Pgrp>,
    root: &str,
    fd: &Arc<Chan>,
    platform: &Arc<dyn Platform>,
    opts: &Opts,
) -> KResult<()> {
    if opts.noauth && opts.encrypt {
        return Err(Error::new(error::EBADARG));
    }
    let mut fs = Export::new(pgrp, root)?;
    fs.rdonly = opts.rdonly;
    let conn = Arc::new(Conn {
        fs,
        fd: fd.clone(),
        platform: platform.clone(),
        noauth: opts.noauth,
        encrypt: opts.encrypt,
        user: opts.user.clone(),
        keyfile: opts.keyfile.clone(),
        pid: platform.current_pid(),
        msize: AtomicU32::new(purg_styx::MAX_MSIZE),
        afids: Mutex::new(BTreeMap::new()),
        flights: Mutex::new(BTreeMap::new()),
        quiet: Rendez::new(),
        work: WorkQ::new(MAX_WORKERS, KEEP_IDLE),
        out: OutQ::new(),
        bufs: Pool::new(),
        switch: Mutex::new(None),
        keys: Mutex::new(None),
        server: Mutex::new(None),
        sealer: Mutex::new(None),
        wnote: Arc::new(Note::new()),
        dead: AtomicBool::new(false),
    });
    let result = conn.read_requests();
    conn.shutdown();
    result
}

impl Conn {
    /// The reader: runs on the exporting prog's thread until the client
    /// hangs up or the prog is killed.
    fn read_requests(self: &Arc<Self>) -> KResult<()> {
        // The client's direction, once a version 3 proof has been read.
        let mut opener: Option<Opener> = None;
        loop {
            let msize = self.msize.load(Ordering::Acquire);
            let got = match opener.as_mut() {
                Some(o) => read_sealed_in(&self.fd, o, msize, Some(&self.bufs)),
                None => read_msg_in(&self.fd, msize, Some(&self.bufs)),
            };
            let buf = match got {
                Ok(b) => b,
                Err(e) if e.as_str() == error::EHUNGUP || e.as_str() == error::EINTR => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let msg = match Msg::decode(&buf, msize) {
                Ok(m) => m,
                Err(_) => return Err(Error::new(error::EIO)),
            };
            let tag = msg.tag;
            if opener.is_none()
                && !self.noauth
                && let Some(keys) = self.proof_keys(&msg)
            {
                // The client seals everything after its version 3 proof,
                // and opens everything after the proof's reply.
                let (sealer, o) = keys.server();
                *self.switch.lock() = Some((tag, sealer));
                *self.keys.lock() = Some(keys);
                opener = Some(o);
            }
            let for_worker = match &msg.fcall {
                Fcall::Tversion { .. } => {
                    if !self.reset() {
                        return Ok(());
                    }
                    let mut m = msize;
                    let reply = dispatch(&mut View(&self.fs), &msg, &mut m);
                    self.msize.store(m, Ordering::Release);
                    self.send(reply);
                    false
                }
                Fcall::Tflush { oldtag } => {
                    self.flush(*oldtag, tag);
                    false
                }
                _ => true,
            };
            drop(msg);
            if for_worker {
                self.submit(Req { tag, buf });
            } else {
                self.bufs.put(buf);
            }
            // A reply could not be written: the client has gone, as when
            // a read hangs up. Clients send `Tclunk` and need not wait for
            // `Rclunk`, so a client that unmounts usually hangs up before
            // the last replies are written.
            if self.dead.load(Ordering::Acquire) {
                return Ok(());
            }
        }
    }

    /// Hands a request to a worker, starting one if none is free.
    fn submit(self: &Arc<Self>, req: Req) {
        let tag = req.tag;
        {
            let mut flights = self.flights.lock();
            if flights.contains_key(&tag) {
                drop(flights);
                self.send(rerr(tag, "duplicate tag"));
                return;
            }
            flights.insert(
                tag,
                Flight {
                    note: Arc::new(Note::new()),
                    flushes: Vec::new(),
                    aborted: false,
                },
            );
        }
        if self.work.put(req) == Put::Queued {
            return;
        }
        let conn = self.clone();
        let started = self.platform.spawn_kproc(
            "export",
            Box::new(move || {
                conn.platform.set_current_pid(conn.pid);
                while let Some(req) = conn.work.take() {
                    conn.run(req, false);
                }
            }),
        );
        if started.is_err() && self.work.unstarted() == 0 {
            // No kproc to be had and none running: serve what is queued
            // here, one request at a time, as a prog without workers
            // would. The exporting prog's own note stays in force.
            while let Some(req) = self.work.try_take() {
                self.run(req, true);
            }
        }
    }

    /// Runs one request and sends its reply. A worker runs it under the
    /// request's note; `inline` keeps the caller's.
    fn run(&self, req: Req, inline: bool) {
        let note = self.flights.lock().get(&req.tag).map(|f| f.note.clone());
        let Some(note) = note else {
            return;
        };
        if !inline {
            sync::set_current_note(Some(note));
        }
        let reply = self.handle(&req);
        if !inline {
            sync::set_current_note(None);
        }
        self.finish(req.tag, reply);
        self.bufs.put(req.buf);
    }

    fn handle(&self, req: &Req) -> Vec<u8> {
        let mut msize = self.msize.load(Ordering::Acquire);
        let Ok(msg) = Msg::decode(&req.buf, msize) else {
            return rerr(req.tag, error::EIO);
        };
        if !self.noauth
            && let Some(bytes) = auth_reply(self, &msg)
        {
            return bytes;
        }
        dispatch(&mut View(&self.fs), &msg, &mut msize)
    }

    /// Ends a request: queues its reply and the `Rflush` of every `Tflush`
    /// that named it. Both are queued under the lock [`Conn::flush`] looks
    /// the tag up with, so an `Rflush` never overtakes the reply.
    fn finish(&self, tag: u16, reply: Vec<u8>) {
        let quiet = {
            let mut flights = self.flights.lock();
            let Some(f) = flights.remove(&tag) else {
                return;
            };
            if !f.aborted {
                // A flushed request that was interrupted has no answer of
                // its own: the client is told it was flushed.
                if f.flushes.is_empty() || !interrupted(&reply) {
                    self.out.push(Out::Msg(reply));
                }
                for t in f.flushes {
                    self.out.push(Out::Msg(rflush(t)));
                }
            }
            // The reply to a version 3 proof is the last one in the clear.
            let mut switch = self.switch.lock();
            if switch.as_ref().is_some_and(|(t, _)| *t == tag)
                && let Some((_, sealer)) = switch.take()
            {
                self.out.push(Out::Key(Box::new(sealer)));
            }
            drop(switch);
            flights.is_empty()
        };
        if quiet {
            self.quiet.wakeup();
        }
        self.write_replies();
    }

    /// `Tflush`: interrupts the request with `oldtag`, if a worker still
    /// has it, and has its end answer this flush. Otherwise the request
    /// was answered already (or never seen) and `Rflush` goes out now.
    fn flush(&self, oldtag: u16, tag: u16) {
        let note = {
            let mut flights = self.flights.lock();
            match flights.get_mut(&oldtag) {
                Some(f) => {
                    f.flushes.push(tag);
                    Some(f.note.clone())
                }
                None => {
                    self.out.push(Out::Msg(rflush(tag)));
                    None
                }
            }
        };
        match note {
            Some(note) => note.kill(),
            None => self.write_replies(),
        }
    }

    /// `Tversion` starts the conversation again: outstanding requests are
    /// aborted and every fid is clunked. Returns false when the exporting
    /// prog was killed while waiting for the workers.
    fn reset(&self) -> bool {
        let notes: Vec<Arc<Note>> = self
            .flights
            .lock()
            .values_mut()
            .map(|f| {
                f.aborted = true;
                f.note.clone()
            })
            .collect();
        for n in notes {
            n.kill();
        }
        if park(&self.quiet, || self.flights.lock().is_empty()).is_err() {
            return false;
        }
        let afids = core::mem::take(&mut *self.afids.lock());
        drop(afids);
        self.fs.clunk_all();
        true
    }

    fn send(&self, reply: Vec<u8>) {
        self.out.push(Out::Msg(reply));
        self.write_replies();
    }

    /// The session keys when `msg` is a version 3 proof written to one of
    /// this connection's auth fids. The proof is checked later, by the
    /// worker that answers it; the keys only need its client key.
    fn proof_keys(&self, msg: &Msg<'_>) -> Option<purg_auth::session::Keys> {
        let Fcall::Twrite { fid, data, .. } = &msg.fcall else {
            return None;
        };
        if data.first() != Some(&purg_auth::proof::V3) {
            return None;
        }
        let afids = self.afids.lock();
        let a = afids.get(fid)?;
        a.kx.keys(&a.uname, &a.aname, data)
    }

    /// This server's proof for the conversation's keys (`purg_auth::server`),
    /// signed with the exporting user's key on the first call, with no
    /// lock held. Empty when the conversation is not encrypted or there is
    /// no key.
    fn server_proof(&self) -> Vec<u8> {
        if let Some(p) = self.server.lock().clone() {
            return p;
        }
        let Some(keys) = self.keys.lock().clone() else {
            return Vec::new();
        };
        let p =
            crate::auth::server_proof(&self.fs.pgrp, &self.user, self.keyfile.as_deref(), &keys)
                .unwrap_or_default();
        if !p.is_empty() {
            *self.server.lock() = Some(p.clone());
        }
        p
    }

    /// Writes queued replies, unless another thread is already writing.
    /// The drainer is the only writer, so it seals them in queue order.
    fn write_replies(&self) {
        self.out.drain(|item| {
            let bytes = match item {
                Out::Msg(b) => b,
                Out::Key(s) => {
                    *self.sealer.lock() = Some(*s);
                    return;
                }
            };
            if self.dead.load(Ordering::Acquire) {
                return;
            }
            // A worker has no note of its own between requests.
            let borrowed = sync::current_note().is_none();
            if borrowed {
                sync::set_current_note(Some(self.wnote.clone()));
            }
            // Sealed into the sealer's own buffer, and written from it with
            // the lock held: the drainer is the only one that takes it.
            let ok = match self.sealer.lock().as_mut() {
                Some(s) => s
                    .seal_buf(&bytes)
                    .is_ok_and(|b| write_full(&self.fd, b).is_ok()),
                None => write_full(&self.fd, &bytes).is_ok(),
            };
            if !ok {
                self.dead.store(true, Ordering::Release);
            }
            if borrowed {
                sync::set_current_note(None);
            }
        });
    }

    /// Interrupts every request, waits for the workers to exit and closes
    /// every fid.
    fn shutdown(&self) {
        self.dead.store(true, Ordering::Release);
        drop(self.work.close());
        let notes: Vec<Arc<Note>> = self
            .flights
            .lock()
            .values()
            .map(|f| f.note.clone())
            .collect();
        for n in notes {
            n.kill();
        }
        self.wnote.kill();
        self.work.join();
        let flights = core::mem::take(&mut *self.flights.lock());
        drop(flights);
        let afids = core::mem::take(&mut *self.afids.lock());
        drop(afids);
        self.fs.clunk_all();
    }
}

fn rflush(tag: u16) -> Vec<u8> {
    reply(tag, Fcall::Rflush)
}

/// Whether `reply` is the `Rerror` of an interrupted request.
fn interrupted(reply: &[u8]) -> bool {
    // size[4] type[1] tag[2] ename[s]
    reply.get(4) == Some(&purg_styx::kind::RERROR)
        && reply.get(9..) == Some(error::EINTR.as_bytes())
}

/// What the reply queue carries.
enum Out {
    /// A reply.
    Msg(Vec<u8>),
    /// Seal every reply after this one.
    Key(Box<Sealer>),
}

struct Afid {
    /// The challenge and this server's ephemeral key for the fid.
    kx: purg_auth::session::Server,
    uname: String,
    aname: String,
    ok: bool,
    /// The proof accepted was version 3.
    v3: bool,
}

/// Reads the trust material for `uname` from the exporter's own namespace
/// (`fs.pgrp`), at absolute paths. Nothing is read from under the exported
/// tree, so a client that can write there cannot add a key or a signer.
fn trust(fs: &Export, uname: &str) -> crate::auth::Trust {
    crate::auth::Trust::client(&fs.pgrp, uname)
}

fn reply(tag: u16, fcall: Fcall<'_>) -> Vec<u8> {
    Msg { tag, fcall }.encode().unwrap_or_else(|_| Vec::new())
}

fn rerr(tag: u16, ename: &'static str) -> Vec<u8> {
    reply(tag, Fcall::Rerror { ename })
}

/// What the auth fid `fid` reads as at `offset`: the offer (the challenge,
/// then the ephemeral key; a version 2 client reads only the challenge),
/// and after it, once a version 3 proof on it is accepted, this server's
/// own proof for the conversation's keys. `None` when `fid` is no auth fid.
fn auth_read(conn: &Conn, fid: u32, offset: u64) -> Option<Vec<u8>> {
    let (offer, v3) = conn.afids.lock().get(&fid).map(|a| (a.kx.offer, a.v3))?;
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if let Some(d) = offer.get(start..).filter(|d| !d.is_empty()) {
        return Some(d.to_vec());
    }
    let start = start - offer.len();
    let proof = if v3 { conn.server_proof() } else { Vec::new() };
    Some(proof.get(start..).unwrap_or(&[]).to_vec())
}

/// Handles `Tauth`, the auth fid and `Tattach` when this export
/// authenticates. `None` means the message belongs to the file tree.
///
/// The auth fids are locked only to look at or change the table: the trust
/// files are read, and a proof is checked or made, with no lock held.
fn auth_reply(conn: &Conn, msg: &Msg<'_>) -> Option<Vec<u8>> {
    let (afids, platform, fs) = (&conn.afids, conn.platform.as_ref(), &conn.fs);
    let tag = msg.tag;
    if let Fcall::Tauth { afid, uname, aname } = &msg.fcall {
        // Nothing trusted: nobody can prove anything, and nobody gets in.
        if !trust(fs, uname).required() {
            return Some(rerr(tag, crate::auth::NO_TRUST));
        }
        let afid = *afid;
        let mut challenge = [0u8; crate::auth::CHALLENGE];
        platform.entropy(&mut challenge);
        let mut random = [0u8; 32];
        platform.entropy(&mut random);
        let kx = purg_auth::session::Server::new(challenge, random);
        let mut afids = afids.lock();
        if afid == NOFID || fs.has_fid(afid) || afids.contains_key(&afid) {
            return Some(rerr(tag, "fid in use"));
        }
        afids.insert(
            afid,
            Afid {
                kx,
                uname: String::from(*uname),
                aname: String::from(*aname),
                ok: false,
                v3: false,
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
    // Each look at the table is a statement of its own, so its lock is
    // released before anything else happens.
    if let Fcall::Tread { fid, offset, count } = &msg.fcall
        && let Some(data) = auth_read(conn, *fid, *offset)
    {
        let n = (*count as usize).min(data.len());
        return Some(reply(tag, Fcall::Rread { data: &data[..n] }));
    }
    if let Fcall::Twrite { fid, data, .. } = &msg.fcall {
        let held = afids
            .lock()
            .get(fid)
            .map(|a| (a.kx.offer, a.uname.clone(), a.aname.clone()));
        if let Some((offer, uname, aname)) = held {
            // The proof is one write.
            let now = platform.now_ns() / 1_000_000_000;
            let t = trust(fs, &uname);
            let v3 = purg_auth::proof::parse_v3(data).is_some();
            let checked = if conn.encrypt && !v3 {
                Err(crate::auth::ENCRYPTION_REQUIRED)
            } else {
                purg_auth::proof::check(&t.policy(now), &offer, &uname, &aname, data)
            };
            // The fid may have been clunked meanwhile, or clunked and made
            // again with another challenge.
            match afids.lock().get_mut(fid) {
                Some(a) if a.kx.offer == offer => {
                    a.ok = checked.is_ok();
                    a.v3 = a.ok && v3;
                }
                _ => return Some(rerr(tag, UNKNOWN_FID)),
            }
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
    }
    if let Fcall::Tclunk { fid } = &msg.fcall {
        let gone = afids.lock().remove(fid).is_some();
        if gone {
            return Some(reply(tag, Fcall::Rclunk));
        }
    }
    if let Fcall::Tattach {
        afid, uname, aname, ..
    } = &msg.fcall
    {
        // With `encrypt`, only an attach on a version 3 proof's fid. An
        // attach without a proof gets nowhere.
        let proved = afids
            .lock()
            .get(afid)
            .map(|a| a.ok && a.uname == *uname && a.aname == *aname && (!conn.encrypt || a.v3));
        return match proved {
            Some(true) => None,
            Some(false) => Some(rerr(tag, crate::auth::REQUIRED)),
            None if !trust(fs, uname).required() => Some(rerr(tag, crate::auth::NO_TRUST)),
            None => Some(rerr(tag, crate::auth::REQUIRED)),
        };
    }
    None
}

/// The exported tree and the connection's fids.
///
/// Every method takes `&self`: the fid table is locked to look a fid up or
/// to change the table, never across a device call, which may block.
struct Export {
    pgrp: Arc<Pgrp>,
    root: String,
    fids: Mutex<BTreeMap<u32, Arc<Fid>>>,
    uqids: Arc<Mutex<Uqids>>,
    iounit: AtomicU32,
    /// Refuse every request that would change the tree.
    rdonly: bool,
}

const UNKNOWN_FID: &str = "unknown fid";
const FID_IN_USE: &str = "fid in use";
/// Inferno's export answers these to a walk, and to an open or create, of
/// a fid that is already open.
const WALK_OPEN: &str = "walk of open fid";
const OPEN_MODE: &str = "open/create -- unknown mode";

impl Export {
    fn new(pgrp: Arc<Pgrp>, root: &str) -> KResult<Export> {
        let chan = pgrp.namec(root, "/")?;
        if !chan.is_dir() {
            return Err(Error::new(error::ENOTDIR));
        }
        Ok(Export {
            root: chan.name.clone(),
            pgrp,
            fids: Mutex::new(BTreeMap::new()),
            uqids: Arc::new(Mutex::new(Uqids::default())),
            iounit: AtomicU32::new(8192),
            rdonly: false,
        })
    }

    /// Walks one name from `from`, which `path` names. A name is walked
    /// from the Chan itself, one device walk ([`Pgrp::walk1`]), so a walk
    /// of n names is n device walks. `..` is resolved from its name in
    /// full, which keeps it inside the export and takes it back across a
    /// mount point the way it came.
    fn step(&self, from: &Chan, path: &str, name: &str) -> KResult<Chan> {
        if name == "." {
            return Ok(from.clone_unopened());
        }
        if name != ".." && !name.contains('/') {
            return self.pgrp.walk1(from, name);
        }
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

    fn has_fid(&self, fid: u32) -> bool {
        self.fids.lock().contains_key(&fid)
    }

    /// The caller's own reference to what `fid` names.
    fn fid(&self, fid: u32) -> Result<Arc<Fid>, String> {
        self.fids
            .lock()
            .get(&fid)
            .cloned()
            .ok_or_else(|| String::from(UNKNOWN_FID))
    }

    /// Makes `fid` name `new`. A fid cannot be made twice.
    fn insert(&self, fid: u32, new: Fid) -> Result<(), String> {
        let mut fids = self.fids.lock();
        if fids.contains_key(&fid) {
            drop(fids);
            // `new` is dropped with the lock released: closing it may
            // wait for a file server.
            return Err(String::from(FID_IN_USE));
        }
        fids.insert(fid, Arc::new(new));
        Ok(())
    }

    /// Puts `new` in the place of `old`, which `fid` must still name: a
    /// concurrent `Tclunk` or `Twalk` may have changed it.
    fn replace(&self, fid: u32, old: &Arc<Fid>, new: Fid) -> Result<(), String> {
        let previous = {
            let mut fids = self.fids.lock();
            match fids.get_mut(&fid) {
                Some(slot) if Arc::ptr_eq(slot, old) => core::mem::replace(slot, Arc::new(new)),
                _ => {
                    drop(fids);
                    return Err(String::from(UNKNOWN_FID));
                }
            }
        };
        drop(previous);
        Ok(())
    }

    /// Clunks every fid.
    fn clunk_all(&self) {
        let fids = core::mem::take(&mut *self.fids.lock());
        drop(fids);
    }

    fn attach(&self, fid: u32, aname: &str) -> Result<Qid, String> {
        if self.has_fid(fid) {
            return Err(String::from(FID_IN_USE));
        }
        let path = if aname.is_empty() {
            self.root.clone()
        } else {
            absolute(aname, &self.root)
        };
        let chan = self.pgrp.namec(&path, "/").map_err(|e| e.to_string())?;
        let uqid = Uqid::new(&self.uqids, &chan);
        let qid = uqid.qid(chan.qid);
        self.insert(
            fid,
            Fid {
                path: chan.name.clone(),
                chan,
                uqid,
            },
        )?;
        Ok(qid)
    }

    fn walk(&self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.has_fid(newfid) {
            return Err(String::from(FID_IN_USE));
        }
        let base = self.fid(fid)?;
        // 9P2000 forbids walking from an open fid, a clone included, and
        // Inferno's export refuses it.
        if base.chan.open {
            return Err(String::from(WALK_OPEN));
        }
        let mut path = base.path.clone();
        let mut chan = base.chan.clone_unopened();
        let mut qids = Vec::new();
        // The files walked through keep their paths until the walk ends.
        let mut uqids = Vec::new();
        for (i, name) in names.iter().enumerate() {
            match self.step(&chan, &path, name) {
                Ok(c) => {
                    let uqid = Uqid::new(&self.uqids, &c);
                    qids.push(uqid.qid(c.qid));
                    uqids.push(uqid);
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
        let uqid = match uqids.pop() {
            Some(u) => u,
            None => Uqid::new(&self.uqids, &chan),
        };
        let new = Fid { path, chan, uqid };
        if newfid == fid {
            self.replace(fid, &base, new)?;
        } else {
            self.insert(newfid, new)?;
        }
        Ok(qids)
    }

    fn open(&self, fid: u32, mode: u8) -> Result<(Qid, u32), String> {
        let access = mode & mode::ACCESS;
        if (access != mode::OREAD && access != mode::OEXEC)
            || mode & (mode::OTRUNC | mode::ORCLOSE) != 0
        {
            self.writable()?;
        }
        let f = self.fid(fid)?;
        if f.chan.open {
            return Err(String::from(OPEN_MODE));
        }
        let opened = open(f.chan.clone_unopened(), mode).map_err(|e| e.to_string())?;
        let uqid = Uqid::new(&self.uqids, &opened);
        let qid = uqid.qid(opened.qid);
        self.replace(
            fid,
            &f,
            Fid {
                path: f.path.clone(),
                chan: opened,
                uqid,
            },
        )?;
        Ok((qid, self.iounit.load(Ordering::Acquire)))
    }

    fn create(&self, fid: u32, name: &str, perm: u32, mode: u8) -> Result<(Qid, u32), String> {
        self.writable()?;
        let f = self.fid(fid)?;
        if f.chan.open {
            return Err(String::from(OPEN_MODE));
        }
        let mut path = f.path.clone();
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(name);
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
        let uqid = Uqid::new(&self.uqids, &opened);
        let qid = uqid.qid(opened.qid);
        self.replace(
            fid,
            &f,
            Fid {
                path: opened.name.clone(),
                chan: opened,
                uqid,
            },
        )?;
        Ok((qid, self.iounit.load(Ordering::Acquire)))
    }

    fn read(&self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
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

    fn write(&self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        self.writable()?;
        let f = self.fid(fid)?;
        let n = f
            .chan
            .dev
            .write(&f.chan, data, offset)
            .map_err(|e| e.to_string())?;
        u32::try_from(n).map_err(|_| String::from(error::EIO))
    }

    fn clunk(&self, fid: u32) -> Result<(), String> {
        let f = self.fids.lock().remove(&fid);
        // Dropped here, with the table unlocked. A request still using the
        // fid keeps the file open until it ends.
        f.map(drop).ok_or_else(|| String::from(UNKNOWN_FID))
    }

    fn remove(&self, fid: u32) -> Result<(), String> {
        // Tremove clunks the fid even when the remove is refused.
        let f = self
            .fids
            .lock()
            .remove(&fid)
            .ok_or_else(|| String::from(UNKNOWN_FID))?;
        self.writable()?;
        f.chan.dev.remove(&f.chan).map_err(|e| e.to_string())
    }

    fn stat(&self, fid: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?;
        let mut d = f.chan.dev.stat(&f.chan).map_err(|e| e.to_string())?;
        d.qid = f.uqid.qid(d.qid);
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&self, fid: u32, stat: &[u8]) -> Result<(), String> {
        self.writable()?;
        let d = Dir::unpack(stat)
            .map(|(d, _)| d)
            .map_err(|e| e.to_string())?;
        let f = self.fid(fid)?;
        f.chan.dev.wstat(&f.chan, &d).map_err(|e| e.to_string())
    }
}

/// [`Export`] as the `StyxFs` that [`dispatch`] drives. Each worker makes
/// its own for the request it is serving.
struct View<'a>(&'a Export);

impl StyxFs for View<'_> {
    fn set_msize(&mut self, msize: u32) {
        let iounit = msize.saturating_sub(purg_styx::IOHDRSZ).max(1);
        self.0.iounit.store(iounit, Ordering::Release);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, aname: &str) -> Result<Qid, String> {
        self.0.attach(fid, aname)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        self.0.walk(fid, newfid, names)
    }

    fn open(&mut self, fid: u32, mode: u8) -> Result<(Qid, u32), String> {
        self.0.open(fid, mode)
    }

    fn create(&mut self, fid: u32, name: &str, perm: u32, mode: u8) -> Result<(Qid, u32), String> {
        self.0.create(fid, name, perm, mode)
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        self.0.read(fid, offset, count)
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        self.0.write(fid, offset, data)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.0.clunk(fid)
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.0.remove(fid)
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        self.0.stat(fid)
    }

    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String> {
        self.0.wstat(fid, stat)
    }
}
