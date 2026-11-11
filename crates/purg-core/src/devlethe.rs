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
//! ```
//!
//! dials the server and gets a ticket, so a refused key fails the write
//! with the server's error. A read of the same open `clone` then gives
//! the connection's number `n`, and `#λn` is the tree at `path`, ready to
//! bind. The connection lives as long as something holds a file of it
//! (the open `clone`, or a bind).
//!
//! Nothing here holds server state: a walk is a `stat` of the joined
//! path, a read is a range request, a directory read pages through
//! `readdir`. Connections are pooled and replaceable. A request whose
//! connection fails is sent again on a new one, a few times with a short
//! pause, so a server that restarts costs a reader a delay and not an
//! error; a ticket the server refuses is renewed and the request resent.
//!
//! File data is cached by version: every read still asks the server, but
//! when the block is cached the request is conditional and the reply
//! carries no data unless the file changed.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_abi::error::{EBADARG, EBADUSEFD, EHUNGUP, EINUSE, EISDIR, ENONEXIST, EPERM};
use purg_abi::mode;
use purg_lethe::client::{self, Creds, Failure, Session};
use purg_lethe::msg::{Op, Owned};
use purg_lethe::{NONCE, path, rights};
use purg_platform::Platform;
use purg_platform::net::Conn;
use purg_platform::sync::{Mutex, Rendez};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};

/// The device character.
pub const DC: char = 'λ';

const QROOT: u64 = 0;
const QCLONE: u64 = 1;

/// The cache's block, and the largest read sent to the server.
pub const BLOCK: usize = 8192;
/// A directory page.
pub const PAGE: u32 = 8192;
/// The cache is emptied when it holds more than this.
const CACHE_MAX: usize = 1 << 20;
/// Tries for a request whose connection fails, and the first pause
/// between them; each pause doubles up to [`MAX_PAUSE_NS`].
const ATTEMPTS: u32 = 9;
const FIRST_PAUSE_NS: u64 = 50_000_000;
const MAX_PAUSE_NS: u64 = 1_000_000_000;
/// A directory that changes while it is paged through is listed again
/// from the start, at most this many times.
const LISTINGS: u32 = 3;

/// One cached file: its version and blocks.
struct Cached {
    vers: u32,
    blocks: BTreeMap<u64, Vec<u8>>,
}

#[derive(Default)]
struct Cache {
    files: BTreeMap<String, Cached>,
    bytes: usize,
}

impl Cache {
    fn get(&self, p: &str, block: u64) -> Option<(u32, Vec<u8>)> {
        let f = self.files.get(p)?;
        Some((f.vers, f.blocks.get(&block)?.clone()))
    }

    fn put(&mut self, p: &str, vers: u32, block: u64, data: &[u8]) {
        if self.bytes + data.len() > CACHE_MAX {
            self.files.clear();
            self.bytes = 0;
        }
        let f = self.files.entry(String::from(p)).or_insert(Cached {
            vers,
            blocks: BTreeMap::new(),
        });
        if f.vers != vers {
            // The file changed: every block of the old version goes.
            let freed: usize = f.blocks.values().map(Vec::len).sum();
            self.bytes -= freed;
            f.blocks.clear();
            f.vers = vers;
        }
        if let Some(old) = f.blocks.insert(block, data.to_vec()) {
            self.bytes -= old.len();
        }
        self.bytes += data.len();
    }
}

/// What the `clone` write said.
struct Settings {
    host: String,
    port: u16,
    creds: Creds,
    prefix: String,
}

fn parse(text: &str) -> Result<Settings> {
    let bad = || Error::new(EBADARG);
    let (mut dial, mut user, mut seed, mut cert, mut prefix) = (None, None, None, None, None);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
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
    Ok(Settings {
        host: String::from(host.ok_or_else(bad)?),
        port: port.and_then(|p| p.parse().ok()).ok_or_else(bad)?,
        creds: Creds {
            user: user.ok_or_else(bad)?,
            seed: seed.ok_or_else(bad)?,
            cert,
        },
        prefix,
    })
}

/// One connection's worth of settings, its ticket and its cache.
pub struct Remote {
    id: u32,
    platform: Arc<dyn Platform>,
    settings: Settings,
    session: Mutex<Option<Session>>,
    idle: Mutex<Vec<Arc<dyn Conn>>>,
    cache: Mutex<Cache>,
    tag: AtomicU32,
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

fn exchange(c: &dyn Conn, frame: &[u8]) -> core::result::Result<Vec<u8>, String> {
    write_all(c, frame)?;
    client::read_frame::<String>(&mut |b| read_exact(c, b))
}

impl Remote {
    fn now(&self) -> u64 {
        self.platform.now_ns() / 1_000_000_000
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
    fn session(&self, c: &dyn Conn) -> core::result::Result<Session, alloc::boxed::Box<Attempt>> {
        if let Some(s) = self.session.lock().as_ref()
            && !s.expiring(self.now())
        {
            return Ok(s.clone());
        }
        let mut random = [0u8; 32];
        self.platform.entropy(&mut random);
        let (frame, pending) = client::auth(
            &self.settings.creds,
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

    fn attempt(&self, c: &dyn Conn, op: &Op<'_>) -> Attempt {
        let s = match self.session(c) {
            Ok(s) => s,
            Err(a) => return *a,
        };
        let mut nonce = [0u8; NONCE];
        self.platform.entropy(&mut nonce);
        let (frame, pending) = s.request(self.tag(), self.now(), op, nonce);
        let reply = match exchange(c, &frame) {
            Ok(r) => r,
            Err(e) => return Attempt::Transport(e),
        };
        match s.reply(&pending, &reply) {
            Ok(o) => Attempt::Done(Ok(o)),
            Err(Failure::Clear(e)) if purg_lethe::error::renew(&e) => Attempt::Renew,
            Err(f) => Attempt::Done(Err(String::from(f.text()))),
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

    fn stat(&self, p: &str) -> Result<Dir> {
        self.stat_tries(p, ATTEMPTS)
    }

    fn stat_tries(&self, p: &str, attempts: u32) -> Result<Dir> {
        match self.call_tries(&Op::Stat { path: p }, attempts)? {
            Owned::Stat(d) => Ok(self.fix(d)),
            _ => Err(Error::new(purg_lethe::error::MESSAGE)),
        }
    }

    fn fix(&self, mut d: Dir) -> Dir {
        d.dtype = DC as u16;
        d.dev = self.id;
        d
    }

    /// Reads one block, from the cache when the server says it is current.
    fn block(&self, p: &str, b: u64) -> Result<Vec<u8>> {
        let cached = self.cache.lock().get(p, b);
        let op = Op::Read {
            path: p,
            offset: b * BLOCK as u64,
            count: BLOCK as u32,
            have: cached.as_ref().map(|c| c.0),
        };
        match self.call(&op)? {
            Owned::Read {
                unchanged: true, ..
            } => match cached {
                Some((_, data)) => Ok(data),
                None => Err(Error::new(purg_lethe::error::MESSAGE)),
            },
            Owned::Read { qid, data, .. } => {
                self.cache.lock().put(p, qid.vers, b, &data);
                Ok(data)
            }
            _ => Err(Error::new(purg_lethe::error::MESSAGE)),
        }
    }

    fn read(&self, p: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            let at = offset + n as u64;
            let b = at / BLOCK as u64;
            let data = self.block(p, b)?;
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
                    return Err(Error::new(purg_lethe::error::MESSAGE));
                };
                if seen.is_some_and(|q| q != qid) && tries + 1 < LISTINGS {
                    tries += 1;
                    continue 'again;
                }
                seen = Some(qid);
                out.extend(entries.into_iter().map(|d| self.fix(d)));
                if next.is_empty() {
                    return Ok(out);
                }
                cookie = next;
            }
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
            cache: Mutex::new(Cache::default()),
            tag: AtomicU32::new(0),
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

    fn walked(remote: &Arc<Remote>, path: String, d: &Dir) -> Walked {
        Walked {
            qid: d.qid,
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
        Ok(DevLethe::walked(&remote, p, &d))
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
                let p = path::join(dir, name);
                let d = remote.stat(&p)?;
                Ok(DevLethe::walked(remote, p, &d))
            }
            _ => Err(Error::new(ENONEXIST)),
        }
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
