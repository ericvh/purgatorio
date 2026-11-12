//! `#λ`: a Lethe tree as files (`docs/lethe.md`).
//!
//! `#λ` holds one file, `clone`. A write of a connection's settings to it
//!
//! ```text
//! dial tcp!host!port
//! user <name>
//! key <seed hex>
//! cert <pgcert1 line>     (optional)
//! path <subtree>          (optional, default /)
//! cache <ms>              (optional, default 1000)
//! ```
//!
//! dials the server and gets a ticket, so a refused key fails the write
//! with the server's error. With a `noauth` line in place of `user`,
//! `key` and `cert`, the connection uses Lethe's clear mode instead: no
//! ticket, nothing sealed, for a server started with `-A`. A connection
//! keeps the mode it was made with; it never changes it because of what
//! a server answers. A read of the same open `clone` then gives
//! the connection's number `n`, and `#λn` is the tree at `path`, ready to
//! bind. The connection lives as long as something holds a file of it
//! (the open `clone`, or a bind).
//!
//! Nothing here holds server state: a walk is a `walk` (or a `stat`) of
//! the joined path, a read is a range request, a directory read pages
//! through `readdir`. Connections are pooled and replaceable. A request
//! whose connection fails is sent again on a new one, a few times with a
//! short pause, so a server that restarts costs a reader a delay and not
//! an error; a ticket the server refuses is renewed and the request
//! resent.
//!
//! What the device learns is kept for a short time, `cache` milliseconds
//! (Section 14 of `docs/lethe.md`):
//!
//! - **Attributes**: a stat, a walk's qids, a directory listing's entries
//!   answer later walks and stats without a round trip while they are
//!   that young.
//! - **Data**: 32 KiB blocks by path and version, in a bounded LRU. A
//!   block is served without asking while the file's version was
//!   confirmed within the lifetime; otherwise the read is conditional
//!   (the reply carries no data unless the file changed). So a file is
//!   revalidated once per lifetime, not once per block.
//!
//! With `cache 0` every stat and every read asks the server, as NFS with
//! `noac` does. What a readahead fetched for the read in progress is
//! used once even then, and a walk's answer is the walk's.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use purg_abi::error::{EBADARG, EBADUSEFD, EHUNGUP, EINUSE, EISDIR, ENONEXIST, ENOTIMPL, EPERM};
use purg_abi::mode;
use purg_lethe::client::{self, ClearPending, Creds, Failure, ReqPending, Session};
use purg_lethe::msg::{Op, Owned};
use purg_lethe::{MAX_WALK, NONCE, path, rights};
use purg_platform::Platform;
use purg_platform::net::Conn;
use purg_platform::sync::{Mutex, Rendez};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, WalkEnd, Walked, WalkedNames};
use crate::error::{Error, Result};

/// The device character.
pub const DC: char = 'λ';

const QROOT: u64 = 0;
const QCLONE: u64 = 1;

/// The cache's block, and the largest read sent to the server: Lethe's
/// largest `count`.
pub const BLOCK: usize = purg_lethe::MAX_COUNT as usize;
/// A directory page.
pub const PAGE: u32 = purg_lethe::MAX_COUNT;
/// The data cache of one connection holds at most this much; the least
/// recently used blocks go first.
const CACHE_MAX: usize = 4 << 20;
/// At most this many attribute entries are kept.
const ATTRS_MAX: usize = 8192;
/// How long attributes and data are trusted unless `cache` says.
pub const DEFAULT_CACHE_MS: u64 = 1000;
/// Blocks read ahead of a sequential reader, sent with the block it
/// asked for on the same connection.
const READAHEAD: u64 = 4;
/// Tries for a request whose connection fails, and the first pause
/// between them; each pause doubles up to [`MAX_PAUSE_NS`].
const ATTEMPTS: u32 = 9;
const FIRST_PAUSE_NS: u64 = 50_000_000;
const MAX_PAUSE_NS: u64 = 1_000_000_000;
/// A directory that changes while it is paged through is listed again
/// from the start, at most this many times.
const LISTINGS: u32 = 3;

/// What is known about one path.
#[derive(Clone)]
struct Known {
    qid: Qid,
    /// Its whole entry, when that came too.
    dir: Option<Dir>,
}

struct Attr {
    known: Known,
    /// `monotonic_ns` when the server said so.
    when: u64,
}

/// The attribute cache.
#[derive(Default)]
struct Attrs {
    map: BTreeMap<String, Attr>,
}

fn young(when: u64, now: u64, life: u64) -> bool {
    now >= when && now - when < life
}

impl Attrs {
    /// Makes room for one more, dropping entries older than `life`. When
    /// none are,
    /// `evict` drops the older half (what a walk or a stat learnt is
    /// wanted now); without it nothing is added (`false`), as for a
    /// listing's entries, which are only a guess.
    fn room(&mut self, now: u64, life: u64, evict: bool) -> bool {
        if self.map.len() < ATTRS_MAX {
            return true;
        }
        self.map.retain(|_, a| young(a.when, now, life));
        if self.map.len() < ATTRS_MAX {
            return true;
        }
        if !evict {
            return false;
        }
        let mut whens: Vec<u64> = self.map.values().map(|a| a.when).collect();
        let mid = whens.len() / 2;
        let (_, cut, _) = whens.select_nth_unstable(mid);
        let cut = *cut;
        self.map.retain(|_, a| a.when > cut);
        self.map.len() < ATTRS_MAX
    }

    fn put(&mut self, p: &str, known: Known, now: u64, life: u64) {
        if let Some(a) = self.map.get_mut(p) {
            *a = Attr { known, when: now };
        } else if self.room(now, life, true) {
            self.map.insert(String::from(p), Attr { known, when: now });
        }
    }

    /// For a walk: the qid, if young.
    fn walk(&self, p: &str, now: u64, life: u64) -> Option<Qid> {
        let a = self.map.get(p)?;
        young(a.when, now, life).then_some(a.known.qid)
    }

    /// For a stat: the entry, if young.
    fn stat(&self, p: &str, now: u64, life: u64) -> Option<Dir> {
        let a = self.map.get(p)?;
        match &a.known {
            Known { dir: Some(d), .. } if young(a.when, now, life) => Some(d.clone()),
            _ => None,
        }
    }

    /// The version confirmed within `life`, and the length if the entry
    /// is known (perhaps older).
    fn version(&self, p: &str, now: u64, life: u64) -> (Option<u32>, Option<u64>) {
        match self.map.get(p) {
            Some(Attr {
                known: Known { qid, dir },
                when,
                ..
            }) => (
                young(*when, now, life).then_some(qid.vers),
                dir.as_ref().map(|d| d.length),
            ),
            _ => (None, None),
        }
    }

    /// The server just gave `qid` for `p` on a read: the version is
    /// current, and the entry is kept only if it is for that version.
    fn confirm(&mut self, p: &str, qid: Qid, now: u64, life: u64) {
        let dir = match self.map.get(p) {
            Some(Attr {
                known: Known { dir: Some(d), .. },
                ..
            }) if d.qid.vers == qid.vers => Some(d.clone()),
            _ => None,
        };
        self.put(p, Known { qid, dir }, now, life);
    }
}

/// One cached block.
struct Block {
    vers: u32,
    data: Vec<u8>,
    /// Its place in the LRU order.
    tick: u64,
    /// Read ahead of the reader and not used yet: good for one use
    /// without asking, however old.
    ahead: bool,
}

/// The data cache: blocks by path and number, least recently used out
/// first.
#[derive(Default)]
struct Cache {
    blocks: BTreeMap<(String, u64), Block>,
    lru: BTreeMap<u64, (String, u64)>,
    bytes: usize,
    tick: u64,
}

impl Cache {
    fn touch(&mut self, key: &(String, u64)) {
        self.tick += 1;
        let t = self.tick;
        if let Some(b) = self.blocks.get_mut(key) {
            self.lru.remove(&b.tick);
            b.tick = t;
            self.lru.insert(t, key.clone());
        }
    }

    /// The block's version and data, marking it used. `ahead` says it was
    /// read ahead and not used before.
    fn get(&mut self, p: &str, b: u64) -> Option<(u32, Vec<u8>, bool)> {
        let key = (String::from(p), b);
        self.touch(&key);
        let blk = self.blocks.get_mut(&key)?;
        let ahead = core::mem::replace(&mut blk.ahead, false);
        Some((blk.vers, blk.data.clone(), ahead))
    }

    fn has(&self, p: &str, b: u64, vers: u32) -> bool {
        self.blocks
            .get(&(String::from(p), b))
            .is_some_and(|blk| blk.vers == vers)
    }

    fn remove(&mut self, key: &(String, u64)) {
        if let Some(b) = self.blocks.remove(key) {
            self.lru.remove(&b.tick);
            self.bytes -= b.data.len();
        }
    }

    fn put(&mut self, p: &str, vers: u32, b: u64, data: &[u8], ahead: bool) {
        // A new version: every block of the old one goes.
        let stale: Vec<(String, u64)> = self
            .blocks
            .range((String::from(p), 0)..=(String::from(p), u64::MAX))
            .filter(|(_, blk)| blk.vers != vers)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            self.remove(k);
        }
        let key = (String::from(p), b);
        self.remove(&key);
        while self.bytes + data.len() > CACHE_MAX {
            let Some((_, oldest)) = self.lru.pop_first() else {
                break;
            };
            if let Some(blk) = self.blocks.remove(&oldest) {
                self.bytes -= blk.data.len();
            }
        }
        self.tick += 1;
        self.lru.insert(self.tick, key.clone());
        self.blocks.insert(
            key,
            Block {
                vers,
                data: data.to_vec(),
                tick: self.tick,
                ahead,
            },
        );
        self.bytes += data.len();
    }
}

/// What the `clone` write said.
struct Settings {
    host: String,
    port: u16,
    /// What to authenticate with; `None` for the clear mode (`noauth`).
    creds: Option<Creds>,
    prefix: String,
    /// How long attributes and data are trusted, in nanoseconds.
    life: u64,
}

fn parse(text: &str) -> Result<Settings> {
    let bad = || Error::new(EBADARG);
    let (mut dial, mut user, mut seed, mut cert, mut prefix) = (None, None, None, None, None);
    let mut noauth = false;
    let mut life = DEFAULT_CACHE_MS * 1_000_000;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line == "noauth" {
            noauth = true;
            continue;
        }
        let (k, v) = line.split_once(' ').ok_or_else(bad)?;
        match k {
            "dial" => dial = Some(v),
            "user" => user = Some(String::from(v)),
            "key" => {
                seed = Some(purg_auth::hex::array::<32>(v.as_bytes()).ok_or_else(bad)?);
            }
            "cert" => {
                cert = Some(purg_auth::cert::Cert::parse(v.as_bytes()).map_err(|_| bad())?);
            }
            "path" => prefix = Some(String::from(v)),
            "cache" => {
                let ms: u64 = v.parse().map_err(|_| bad())?;
                life = ms.checked_mul(1_000_000).ok_or_else(bad)?;
            }
            _ => return Err(bad()),
        }
    }
    let mut parts = dial.ok_or_else(bad)?.split('!');
    let (net, host, port) = (parts.next(), parts.next(), parts.next());
    if net != Some("tcp") || parts.next().is_some() {
        return Err(bad());
    }
    let prefix = prefix.unwrap_or_else(|| String::from("/"));
    if !path::valid(&prefix) {
        return Err(Error::new(purg_abi::error::EFILENAME));
    }
    // The mode is one or the other: `noauth` with a user or a key is a
    // mistake, not a request to try both.
    let creds = if noauth {
        if user.is_some() || seed.is_some() || cert.is_some() {
            return Err(bad());
        }
        None
    } else {
        Some(Creds {
            user: user.ok_or_else(bad)?,
            seed: seed.ok_or_else(bad)?,
            cert,
        })
    };
    Ok(Settings {
        host: String::from(host.ok_or_else(bad)?),
        port: port.and_then(|p| p.parse().ok()).ok_or_else(bad)?,
        creds,
        prefix,
        life,
    })
}

/// One connection's worth of settings, its ticket and its caches.
pub struct Remote {
    id: u32,
    platform: Arc<dyn Platform>,
    settings: Settings,
    session: Mutex<Option<Session>>,
    idle: Mutex<Vec<Arc<dyn Conn>>>,
    attrs: Mutex<Attrs>,
    cache: Mutex<Cache>,
    tag: AtomicU32,
    /// Whether the server takes `Twalk`; one from before it says `not
    /// implemented`, and walks then go a name at a time.
    walks: AtomicBool,
}

/// Why one attempt failed.
enum Attempt {
    /// The connection failed: try another.
    Transport(String),
    /// The ticket was refused: get a new one and resend.
    Renew,
    /// The answer.
    Done(core::result::Result<Owned, String>),
}

/// A request on the wire, waiting for its reply.
enum Waiting {
    Sealed(ReqPending),
    Clear(ClearPending),
}

fn write_all(c: &dyn Conn, mut buf: &[u8]) -> core::result::Result<(), String> {
    while !buf.is_empty() {
        match c.write(buf) {
            Ok(0) => return Err(String::from(EHUNGUP)),
            Ok(n) => buf = &buf[n.min(buf.len())..],
            Err(e) => return Err(String::from(&*e)),
        }
    }
    Ok(())
}

fn read_exact(c: &dyn Conn, buf: &mut [u8]) -> core::result::Result<(), String> {
    let mut n = 0;
    while n < buf.len() {
        match c.read(&mut buf[n..]) {
            Ok(0) => return Err(String::from(EHUNGUP)),
            Ok(k) => n += k,
            Err(e) => return Err(String::from(&*e)),
        }
    }
    Ok(())
}

fn read_frame(c: &dyn Conn) -> core::result::Result<Vec<u8>, String> {
    client::read_frame::<String>(&mut |b| read_exact(c, b))
}

fn exchange(c: &dyn Conn, frame: &[u8]) -> core::result::Result<Vec<u8>, String> {
    write_all(c, frame)?;
    read_frame(c)
}

fn bad() -> Error {
    Error::new(purg_lethe::error::MESSAGE)
}

impl Remote {
    fn now(&self) -> u64 {
        self.platform.now_ns() / 1_000_000_000
    }

    /// The clock the caches go by.
    fn mono(&self) -> u64 {
        self.platform.monotonic_ns()
    }

    fn life(&self) -> u64 {
        self.settings.life
    }

    fn tag(&self) -> u32 {
        self.tag.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// An idle connection, or a new one.
    fn conn(&self) -> core::result::Result<Arc<dyn Conn>, String> {
        if let Some(c) = self.idle.lock().pop() {
            return Ok(c);
        }
        let net = self
            .platform
            .net()
            .ok_or_else(|| String::from("network unavailable"))?;
        net.dial_tcp(&self.settings.host, self.settings.port)
            .map_err(|e| String::from(&*e))
    }

    fn idle(&self, c: Arc<dyn Conn>) {
        self.idle.lock().push(c);
    }

    /// The current ticket, or a new one got over `c`. Fails with the
    /// attempt's outcome: the transport's error, or the server's refusal.
    fn session(
        &self,
        c: &dyn Conn,
        creds: &Creds,
    ) -> core::result::Result<Session, alloc::boxed::Box<Attempt>> {
        if let Some(s) = self.session.lock().as_ref()
            && !s.expiring(self.now())
        {
            return Ok(s.clone());
        }
        let mut random = [0u8; 32];
        self.platform.entropy(&mut random);
        let (frame, pending) = client::auth(
            creds,
            self.tag(),
            rights::READ,
            &self.settings.prefix,
            random,
        );
        let reply =
            exchange(c, &frame).map_err(|e| alloc::boxed::Box::new(Attempt::Transport(e)))?;
        let s = pending
            .finish(&reply, self.now())
            .map_err(|e| alloc::boxed::Box::new(Attempt::Done(Err(e))))?;
        *self.session.lock() = Some(s.clone());
        Ok(s)
    }

    /// The frame for `op` and what its reply needs, in the connection's
    /// mode.
    fn frame(&self, s: Option<&Session>, op: &Op<'_>) -> (Vec<u8>, u32, Waiting) {
        let id = self.tag();
        match s {
            Some(s) => {
                let mut nonce = [0u8; NONCE];
                self.platform.entropy(&mut nonce);
                let (f, p) = s.request(id, self.now(), op, nonce);
                (f, id, Waiting::Sealed(p))
            }
            None => {
                let (f, p) = client::clear(id, op);
                (f, id, Waiting::Clear(p))
            }
        }
    }

    fn reply(s: Option<&Session>, p: &Waiting, frame: &[u8]) -> Attempt {
        let r = match (s, p) {
            (Some(s), Waiting::Sealed(p)) => s.reply(p, frame),
            (_, Waiting::Clear(p)) => p.reply(frame),
            _ => Err(Failure::Bad(purg_lethe::error::MESSAGE)),
        };
        match r {
            Ok(o) => Attempt::Done(Ok(o)),
            Err(Failure::Clear(e)) if s.is_some() && purg_lethe::error::renew(&e) => Attempt::Renew,
            Err(f) => Attempt::Done(Err(String::from(f.text()))),
        }
    }

    /// The ticket for one attempt over `c`: `Ok(None)` in the clear mode.
    fn ticket(
        &self,
        c: &dyn Conn,
    ) -> core::result::Result<Option<Session>, alloc::boxed::Box<Attempt>> {
        match &self.settings.creds {
            None => Ok(None),
            Some(creds) => self.session(c, creds).map(Some),
        }
    }

    fn attempt(&self, c: &dyn Conn, op: &Op<'_>) -> Attempt {
        let s = match self.ticket(c) {
            Ok(s) => s,
            Err(a) => return *a,
        };
        let (frame, _, pending) = self.frame(s.as_ref(), op);
        match exchange(c, &frame) {
            Ok(reply) => Remote::reply(s.as_ref(), &pending, &reply),
            Err(e) => Attempt::Transport(e),
        }
    }

    /// Sends `op` until it is answered.
    fn call(&self, op: &Op<'_>) -> Result<Owned> {
        self.call_tries(op, ATTEMPTS)
    }

    /// Sends `op` until it is answered, trying at most `attempts` connections.
    fn call_tries(&self, op: &Op<'_>, attempts: u32) -> Result<Owned> {
        let mut pause = FIRST_PAUSE_NS;
        let mut renewed = false;
        let mut last = String::from(EHUNGUP);
        let mut tries = 0;
        while tries < attempts {
            let c = match self.conn() {
                Ok(c) => c,
                Err(e) => {
                    last = e;
                    tries += 1;
                    Rendez::new().tsleep(|| false, pause);
                    pause = (pause * 2).min(MAX_PAUSE_NS);
                    continue;
                }
            };
            match self.attempt(&*c, op) {
                Attempt::Done(r) => {
                    self.idle(c);
                    return r.map_err(Error::from);
                }
                Attempt::Renew if !renewed => {
                    renewed = true;
                    *self.session.lock() = None;
                    self.idle(c);
                }
                Attempt::Renew => {
                    self.idle(c);
                    return Err(Error::new(purg_lethe::error::TICKET));
                }
                Attempt::Transport(e) => {
                    c.hangup();
                    last = e;
                    tries += 1;
                    // A pooled connection may simply be stale: try a fresh
                    // one at once before pausing.
                    if tries > 1 {
                        Rendez::new().tsleep(|| false, pause);
                        pause = (pause * 2).min(MAX_PAUSE_NS);
                    }
                }
            }
        }
        Err(Error::from(last))
    }

    /// Sends every op in `ops` on one connection before reading any reply
    /// (replies are matched by id, in whatever order they come). One try
    /// only: `None` if the connection or the ticket fails, and the caller
    /// goes on without these answers.
    fn pipeline(&self, ops: &[Op<'_>]) -> Option<Vec<core::result::Result<Owned, String>>> {
        let c = self.conn().ok()?;
        let s = match self.ticket(&*c) {
            Ok(s) => s,
            Err(a) if matches!(*a, Attempt::Transport(_)) => {
                c.hangup();
                return None;
            }
            Err(_) => {
                self.idle(c);
                return None;
            }
        };
        let mut out = Vec::new();
        let mut waiting = Vec::new();
        for op in ops {
            let (f, id, p) = self.frame(s.as_ref(), op);
            out.extend_from_slice(&f);
            waiting.push((id, p));
        }
        let mut answers: Vec<Option<core::result::Result<Owned, String>>> =
            ops.iter().map(|_| None).collect();
        let mut fail = write_all(&*c, &out).is_err();
        let mut renew = false;
        for _ in 0..ops.len() {
            if fail {
                break;
            }
            let Ok(reply) = read_frame(&*c) else {
                fail = true;
                break;
            };
            let id = purg_lethe::msg::frame_id(&reply);
            let Some(i) = waiting.iter().position(|(w, _)| *w == id) else {
                fail = true;
                break;
            };
            match Remote::reply(s.as_ref(), &waiting[i].1, &reply) {
                Attempt::Done(r) => answers[i] = Some(r),
                // The ticket was refused: the caller's ordinary call gets
                // a new one.
                _ => renew = true,
            }
        }
        if fail {
            c.hangup();
            return None;
        }
        if renew {
            *self.session.lock() = None;
            self.idle(c);
            return None;
        }
        self.idle(c);
        answers.into_iter().collect()
    }

    fn fix(&self, mut d: Dir) -> Dir {
        d.dtype = DC as u16;
        d.dev = self.id;
        d
    }

    fn stat(&self, p: &str) -> Result<Dir> {
        self.stat_tries(p, ATTEMPTS)
    }

    fn stat_tries(&self, p: &str, attempts: u32) -> Result<Dir> {
        if let Some(d) = self.attrs.lock().stat(p, self.mono(), self.life()) {
            return Ok(d);
        }
        match self.call_tries(&Op::Stat { path: p }, attempts)? {
            Owned::Stat(d) => {
                let d = self.fix(d);
                self.found(p, &d);
                Ok(d)
            }
            _ => Err(bad()),
        }
    }

    /// Remembers `d` as `p`'s entry.
    fn found(&self, p: &str, d: &Dir) {
        let known = Known {
            qid: d.qid,
            dir: Some(d.clone()),
        };
        self.attrs.lock().put(p, known, self.mono(), self.life());
    }

    /// The qid of `dir`'s child `name`, from the attribute cache when it
    /// has it, else from the server.
    fn walk(&self, dir: &str, name: &str) -> Result<Qid> {
        let p = path::join(dir, name);
        if let Some(qid) = self.attrs.lock().walk(&p, self.mono(), self.life()) {
            return Ok(qid);
        }
        Ok(self.stat(&p)?.qid)
    }

    /// Walks `names` from `dir`: from the attribute cache when it knows
    /// them all, else in one `Twalk` (or a name with a `stat`, from a
    /// server without `Twalk`). Returns the qids of the names walked and, when
    /// the walk stopped short, why. Fails when the first name cannot be
    /// walked.
    fn walk_names(&self, dir: &str, names: &[&str]) -> Result<(Vec<Qid>, Option<Error>)> {
        let mut qids = Vec::new();
        let mut p = String::from(dir);
        {
            let (now, life) = (self.mono(), self.life());
            let a = self.attrs.lock();
            for n in names.iter().take_while(|n| path::valid_elem(n)) {
                let q = path::join(&p, n);
                let Some(qid) = a.walk(&q, now, life) else {
                    break;
                };
                qids.push(qid);
                p = q;
            }
        }
        if qids.len() == names.len() {
            return Ok((qids, None));
        }
        // The whole walk from `dir`, not the rest from where the cache
        // ends: the same one request, and it answers the last name's
        // entry from one look at the server's tree.
        self.walk_server(dir, names)
    }

    /// The qids of `names` below `dir`, from the server: all in one
    /// `Twalk`, which also answers the last one's entry for the stat that
    /// usually follows. A name that is not valid in a Lethe path is not
    /// there. Fails when the first name cannot be walked.
    fn walk_server(&self, dir: &str, names: &[&str]) -> Result<(Vec<Qid>, Option<Error>)> {
        let wanted: Vec<&str> = names
            .iter()
            .copied()
            .take_while(|n| path::valid_elem(n))
            .take(MAX_WALK)
            .collect();
        let Some(first) = wanted.first() else {
            return Err(Error::new(ENONEXIST));
        };
        let total = wanted.iter().map(|n| n.len() + 1).sum::<usize>() + dir.len();
        if wanted.len() >= 2 && total <= path::MAX_PATH && self.walks.load(Ordering::Relaxed) {
            let op = Op::Walk {
                path: dir,
                names: wanted.clone(),
            };
            match self.call(&op) {
                Ok(Owned::Walk { qids, error, stat }) if qids.len() <= wanted.len() => {
                    let (now, life) = (self.mono(), self.life());
                    let mut a = self.attrs.lock();
                    let mut p = String::from(dir);
                    let n = qids.len();
                    for (i, q) in qids.iter().enumerate() {
                        p = path::join(&p, wanted[i]);
                        let dir = if i + 1 == n && error.is_none() {
                            stat.clone().map(|d| self.fix(d))
                        } else {
                            None
                        };
                        a.put(&p, Known { qid: *q, dir }, now, life);
                    }
                    drop(a);
                    let why = error.map(Error::from);
                    if n == 0 {
                        return Err(why.unwrap_or(Error::new(ENONEXIST)));
                    }
                    let why = (n < names.len()).then(|| why.unwrap_or(Error::new(ENONEXIST)));
                    return Ok((qids, why));
                }
                Ok(_) => return Err(bad()),
                Err(e) if e.as_str() == ENOTIMPL => self.walks.store(false, Ordering::Relaxed),
                Err(e) => return Err(e),
            }
        }
        let qid = self.walk(dir, first)?;
        Ok((alloc::vec![qid], None))
    }

    /// Reads into `buf` at `offset`. Blocks come from the cache while the
    /// file's version is confirmed (within the lifetime, or by this read's
    /// first request); the rest from the server, with readahead when the
    /// reader is sequential.
    fn read(&self, p: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let (mut current, length) = self.attrs.lock().version(p, self.mono(), self.life());
        let mut n = 0;
        while n < buf.len() {
            let at = offset + n as u64;
            let b = at / BLOCK as u64;
            let cached = self.cache.lock().get(p, b);
            let data = match cached {
                // A block read ahead is good once, unless the file has
                // since been seen at another version.
                Some((vers, data, ahead))
                    if current == Some(vers) || (ahead && current.is_none()) =>
                {
                    current = Some(vers);
                    data
                }
                cached => {
                    let (vers, data) = self.fetch(p, b, cached, length)?;
                    current = Some(vers);
                    data
                }
            };
            let skip = (at - b * BLOCK as u64) as usize;
            if skip >= data.len() {
                break;
            }
            let k = (data.len() - skip).min(buf.len() - n);
            buf[n..n + k].copy_from_slice(&data[skip..skip + k]);
            n += k;
            if data.len() < BLOCK {
                break;
            }
        }
        Ok(n)
    }

    /// Block `b` from the server (conditional when `cached`), and the
    /// blocks after it when the reader looks sequential: it starts at 0 or
    /// the block before is cached. Readahead stops at the file's length
    /// when that is known.
    fn fetch(
        &self,
        p: &str,
        b: u64,
        cached: Option<(u32, Vec<u8>, bool)>,
        length: Option<u64>,
    ) -> Result<(u32, Vec<u8>)> {
        let have = cached.as_ref().map(|c| c.0);
        let op = |blk: u64, have: Option<u32>| Op::Read {
            path: p,
            offset: blk * BLOCK as u64,
            count: BLOCK as u32,
            have,
        };
        let mut ops = alloc::vec![op(b, have)];
        let sequential = match b {
            0 => length.is_some(),
            _ => {
                let c = self.cache.lock();
                c.blocks.contains_key(&(String::from(p), b - 1))
            }
        };
        if sequential {
            let c = self.cache.lock();
            for k in b + 1..=b + READAHEAD {
                if length.is_some_and(|l| k * BLOCK as u64 >= l)
                    || c.blocks.contains_key(&(String::from(p), k))
                {
                    break;
                }
                ops.push(op(k, None));
            }
        }
        let mut answers = match ops.len() {
            1 => None,
            _ => self.pipeline(&ops),
        };
        let first = match answers.as_mut() {
            Some(a) => core::mem::replace(&mut a[0], Err(String::new())).map_err(Error::from)?,
            None => self.call(&ops[0])?,
        };
        let (qid, data) = match first {
            Owned::Read {
                qid,
                unchanged: true,
                ..
            } => match cached {
                Some((v, data, _)) if v == qid.vers => (qid, data),
                _ => return Err(bad()),
            },
            Owned::Read { qid, data, .. } => {
                self.cache.lock().put(p, qid.vers, b, &data, false);
                (qid, data)
            }
            _ => return Err(bad()),
        };
        self.attrs.lock().confirm(p, qid, self.mono(), self.life());
        // The blocks read ahead, if they are of the same version and the
        // file goes that far.
        let full = data.len() == BLOCK;
        if let Some(answers) = answers
            && full
        {
            let mut c = self.cache.lock();
            for (i, a) in answers.into_iter().enumerate().skip(1) {
                let Ok(Owned::Read {
                    qid: q, data: d, ..
                }) = a
                else {
                    break;
                };
                if q.vers != qid.vers {
                    break;
                }
                let k = b + i as u64;
                let last = d.len() < BLOCK;
                if !c.has(p, k, q.vers) {
                    c.put(p, q.vers, k, &d, true);
                }
                if last {
                    break;
                }
            }
        }
        Ok((qid.vers, data))
    }

    fn list(&self, p: &str) -> Result<Vec<Dir>> {
        let mut tries = 0;
        'again: loop {
            let mut out = Vec::new();
            let mut cookie = Vec::new();
            let mut seen: Option<Qid> = None;
            loop {
                let op = Op::Readdir {
                    path: p,
                    cookie: &cookie,
                    count: PAGE,
                };
                let Owned::Readdir {
                    qid,
                    cookie: next,
                    entries,
                } = self.call(&op)?
                else {
                    return Err(bad());
                };
                if seen.is_some_and(|q| q != qid) && tries + 1 < LISTINGS {
                    tries += 1;
                    continue 'again;
                }
                seen = Some(qid);
                out.extend(entries.into_iter().map(|d| self.fix(d)));
                if next.is_empty() {
                    break;
                }
                cookie = next;
            }
            // The entries answer walks and stats of these names for a
            // while, as NFS's READDIRPLUS does.
            if self.life() > 0 {
                let (now, life) = (self.mono(), self.life());
                let mut a = self.attrs.lock();
                for d in &out {
                    let k = path::join(p, &d.name);
                    if !a.map.contains_key(&k) && !a.room(now, life, false) {
                        break;
                    }
                    let known = Known {
                        qid: d.qid,
                        dir: Some(d.clone()),
                    };
                    a.put(&k, known, now, life);
                }
            }
            return Ok(out);
        }
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        for c in self.idle.lock().drain(..) {
            c.hangup();
        }
    }
}

/// What a Chan of this device is.
enum Node {
    /// `#λ` or its `clone`.
    Top,
    /// A file of a connection, by its path on the server.
    File { remote: Arc<Remote>, path: String },
}

/// The open `clone`'s state: the connection its write made.
struct Pending(Option<Arc<Remote>>);

pub struct DevLethe {
    platform: Arc<dyn Platform>,
    remotes: Mutex<Vec<(u32, Weak<Remote>)>>,
    next: AtomicU32,
}

impl DevLethe {
    pub fn new(platform: Arc<dyn Platform>) -> DevLethe {
        DevLethe {
            platform,
            remotes: Mutex::new(Vec::new()),
            next: AtomicU32::new(0),
        }
    }

    fn node<'c>(&self, c: &'c Chan) -> Option<&'c Node> {
        c.aux::<Node>()
    }

    fn file<'c>(&self, c: &'c Chan) -> Result<(&'c Arc<Remote>, &'c str)> {
        match self.node(c) {
            Some(Node::File { remote, path }) => Ok((remote, path)),
            _ => Err(Error::new(ENONEXIST)),
        }
    }

    fn top_dir(&self, q: u64) -> Dir {
        let user = self.platform.user();
        let (name, qtype, mode) = if q == QROOT {
            ("#λ", QTDIR, DMDIR | 0o555)
        } else {
            ("clone", QTFILE, 0o666)
        };
        Dir {
            dtype: DC as u16,
            qid: Qid {
                path: q,
                vers: 0,
                qtype,
            },
            mode,
            name: String::from(name),
            uid: user.clone(),
            gid: user.clone(),
            muid: user,
            ..Dir::default()
        }
    }

    /// Connects with the settings in `text` and returns the connection.
    fn connect(&self, text: &str) -> Result<Arc<Remote>> {
        let settings = parse(text)?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let remote = Arc::new(Remote {
            id,
            platform: self.platform.clone(),
            settings,
            session: Mutex::new(None),
            idle: Mutex::new(Vec::new()),
            attrs: Mutex::new(Attrs::default()),
            cache: Mutex::new(Cache::default()),
            tag: AtomicU32::new(0),
            walks: AtomicBool::new(true),
        });
        // The attach point must be there, and the ticket good. A server
        // that does not answer fails the attach at once: the retries are
        // for one that goes away later.
        let d = remote.stat_tries(&remote.settings.prefix, 1)?;
        if d.mode & DMDIR == 0 {
            return Err(Error::new(purg_abi::error::ENOTDIR));
        }
        let mut r = self.remotes.lock();
        r.retain(|(_, w)| w.strong_count() > 0);
        r.push((id, Arc::downgrade(&remote)));
        Ok(remote)
    }

    fn walked(remote: &Arc<Remote>, path: String, qid: Qid) -> Walked {
        Walked {
            qid,
            aux: Some(Arc::new(Node::File {
                remote: remote.clone(),
                path,
            }) as Arc<dyn Any + Send + Sync>),
        }
    }
}

impl Dev for DevLethe {
    fn dc(&self) -> char {
        DC
    }

    fn name(&self) -> &'static str {
        "lethe"
    }

    fn attach(&self, spec: &str) -> Result<Walked> {
        if spec.is_empty() {
            return Ok(Walked {
                qid: Qid {
                    path: QROOT,
                    vers: 0,
                    qtype: QTDIR,
                },
                aux: Some(Arc::new(Node::Top)),
            });
        }
        let id: u32 = spec.parse().map_err(|_| Error::new(EBADARG))?;
        let remote = self
            .remotes
            .lock()
            .iter()
            .find(|(i, _)| *i == id)
            .and_then(|(_, w)| w.upgrade())
            .ok_or(Error::new(ENONEXIST))?;
        let p = remote.settings.prefix.clone();
        let d = remote.stat(&p)?;
        Ok(DevLethe::walked(&remote, p, d.qid))
    }

    fn devno(&self, spec: &str) -> u32 {
        spec.parse().unwrap_or(0)
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        match self.node(c) {
            Some(Node::Top) if c.qid.path == QROOT && name == "clone" => Ok(Walked {
                qid: Qid {
                    path: QCLONE,
                    vers: 0,
                    qtype: QTFILE,
                },
                aux: Some(Arc::new(Node::Top)),
            }),
            Some(Node::File { remote, path: dir }) => {
                if !path::valid_elem(name) {
                    return Err(Error::new(ENONEXIST));
                }
                let qid = remote.walk(dir, name)?;
                Ok(DevLethe::walked(remote, path::join(dir, name), qid))
            }
            _ => Err(Error::new(ENONEXIST)),
        }
    }

    fn max_walk(&self) -> usize {
        MAX_WALK
    }

    /// A whole path in one `Twalk` (`Remote::walk_names`).
    fn walk_names(&self, c: &Chan, names: &[&str]) -> Result<WalkedNames> {
        let Some(Node::File { remote, path: dir }) = self.node(c) else {
            let w = self.walk(c, names.first().ok_or(Error::new(ENONEXIST))?)?;
            return Ok(WalkedNames {
                qids: alloc::vec![w.qid],
                aux: w.aux,
                end: WalkEnd::Reached,
            });
        };
        let (qids, why) = remote.walk_names(dir, names)?;
        if let Some(why) = why {
            return Ok(WalkedNames {
                qids,
                aux: None,
                end: WalkEnd::Stopped(Some(why)),
            });
        }
        let mut p = dir.clone();
        for n in &names[..qids.len()] {
            p = path::join(&p, n);
        }
        let last = *qids.last().ok_or(Error::new(ENONEXIST))?;
        Ok(WalkedNames {
            aux: DevLethe::walked(remote, p, last).aux,
            qids,
            end: WalkEnd::Reached,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        match self.node(c) {
            Some(Node::File { remote, path }) => remote.stat(path),
            _ => Ok(self.top_dir(c.qid.path)),
        }
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let access = omode & mode::ACCESS;
        let write = access == mode::OWRITE || access == mode::ORDWR;
        match self.node(c) {
            Some(Node::Top) if c.qid.path == QCLONE => {
                *c.state.lock() = Some(alloc::boxed::Box::new(Pending(None)));
                Ok(())
            }
            _ if write || omode & mode::OTRUNC != 0 => {
                Err(Error::new(if c.is_dir() { EISDIR } else { EPERM }))
            }
            _ => Ok(()),
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if let Some(Node::Top) = self.node(c) {
            let state = c.state.lock();
            let id = state
                .as_ref()
                .and_then(|s| s.downcast_ref::<Pending>())
                .and_then(|p| p.0.as_ref())
                .map(|r| alloc::format!("{}", r.id))
                .ok_or(Error::new(EBADUSEFD))?;
            return Ok(crate::dev::read_str(id.as_bytes(), buf, offset));
        }
        let (remote, p) = self.file(c)?;
        remote.read(p, buf, offset)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        match self.node(c) {
            Some(Node::File { remote, path }) => remote.list(path),
            _ => Ok(alloc::vec![self.top_dir(QCLONE)]),
        }
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if !matches!(self.node(c), Some(Node::Top)) || c.qid.path != QCLONE {
            return Err(Error::new(EPERM));
        }
        let text = core::str::from_utf8(buf).map_err(|_| Error::new(EBADARG))?;
        let mut state = c.state.lock();
        let pending = state
            .as_mut()
            .and_then(|s| s.downcast_mut::<Pending>())
            .ok_or(Error::new(EBADUSEFD))?;
        if pending.0.is_some() {
            return Err(Error::new(EINUSE));
        }
        pending.0 = Some(self.connect(text)?);
        Ok(buf.len())
    }

    fn iounit(&self, c: &Chan) -> u32 {
        if matches!(self.node(c), Some(Node::File { .. })) {
            BLOCK as u32
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_cache_is_lru_and_by_version() {
        let mut c = Cache::default();
        let block = alloc::vec![0u8; BLOCK];
        let n = (CACHE_MAX / BLOCK) as u64;
        for b in 0..n {
            c.put("/f", 1, b, &block, false);
        }
        assert_eq!(c.bytes, CACHE_MAX);
        // Block 0 is used, so block 1 is the oldest when one more comes.
        assert!(c.get("/f", 0).is_some());
        c.put("/g", 7, 0, &block, false);
        assert!(c.has("/f", 0, 1) && !c.has("/f", 1, 1) && c.has("/g", 0, 7));
        assert_eq!(c.bytes, CACHE_MAX);
        // A new version of /f drops every block of the old one.
        c.put("/f", 2, 5, b"x", true);
        assert!(!c.has("/f", 0, 1) && c.has("/f", 5, 2));
        assert_eq!(c.bytes, BLOCK + 1);
        // A block read ahead says so once.
        assert_eq!(c.get("/f", 5).map(|b| b.2), Some(true));
        assert_eq!(c.get("/f", 5).map(|b| b.2), Some(false));
        assert_eq!(c.lru.len(), c.blocks.len());
    }

    #[test]
    fn attributes_age_and_make_room() {
        let mut a = Attrs::default();
        let q = Qid::default();
        let found = || Known { qid: q, dir: None };
        a.put("/a", found(), 100, 10);
        assert!(a.walk("/a", 105, 10).is_some());
        assert!(a.walk("/a", 110, 10).is_none());
        // Full of young entries: a listing adds nothing, a walk evicts the
        // older half.
        a.map.clear();
        for i in 0..ATTRS_MAX {
            a.put(&alloc::format!("/{i}"), found(), i as u64, u64::MAX);
        }
        let now = ATTRS_MAX as u64;
        assert!(!a.room(now, u64::MAX, false));
        a.put("/new", found(), now, u64::MAX);
        assert!(a.map.contains_key("/new") && !a.map.contains_key("/0"));
        assert!(a.map.len() <= ATTRS_MAX / 2 + 1);
    }
}
