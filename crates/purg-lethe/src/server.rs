//! A Lethe server: one frame in, one frame out, nothing kept
//! (`docs/lethe.md`). [`Server::handle`] answers a frame from a
//! [`Tree`], the trust files and the clock; the caller moves frames on and
//! off the wire.
//!
//! A server answers sealed requests from clients it gave a ticket. With
//! [`Server::clear`] set (`lethed -A`) it also answers clear requests,
//! which carry no ticket and are not sealed, as the user `none`.

use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::auth::{FAILED, KIND_CERT, KIND_KEY, NO_TRUST, NOT_REQUIRED, PQ_REQUIRED, REQUIRED};
use purg_abi::error::{EISDIR, ENOTDIR, ENOTIMPL, EPERM, ESHORTSTAT};
use purg_auth::proof::{Policy, check_signed};
use purg_auth::session::Ephemeral;
use purg_auth::sig::Id;
use purg_styx::{DMDIR, Dir, Qid, STATFIXLEN};

use crate::crypto::{self, Keys};
use crate::error::{MESSAGE, REQUEST_EXPIRED, TICKET_EXPIRED};
use crate::msg::{self, Frame, Grant, Msg, Op, Reply, Request, Tauth, Undecoded};
use crate::ticket::{self, Ticket, TicketKey};
use crate::{KEM_PUBLIC, MAX_COUNT, NONCE, NONE, path, rights};

/// The tree a server serves. Paths are valid Lethe paths. Errors are
/// Inferno strings. Permission checks are the server's, from the modes
/// and owners the tree reports.
pub trait Tree {
    fn stat(&self, path: &str) -> Result<Dir, String>;
    /// Reads `path` at `offset`; returns the file's qid at the time of
    /// the read and the count.
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(Qid, usize), String>;
    /// The directory's qid and at most `limit` of its entries whose
    /// names sort after `after` (as bytes), in that order. A server is
    /// stateless, so each page of a listing asks again; the limit lets a
    /// tree stat only the entries a page can hold, not the whole
    /// directory every time.
    fn list(&self, path: &str, after: &[u8], limit: usize) -> Result<(Qid, Vec<Dir>), String>;
}

/// The keys a server trusts for one user, read when a `Tauth` comes in.
#[derive(Clone, Default, Debug)]
pub struct Trust {
    /// Keys held directly.
    pub keys: Vec<Id>,
    pub signers: Vec<Id>,
    /// `None` when the list is damaged: every certificate then fails.
    pub revoked: Option<Vec<Id>>,
}

/// A server's settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// How long a ticket lasts, in seconds.
    pub lifetime: u64,
    /// How far a request's time may be from the server's, in seconds.
    pub window: u64,
    /// The rights this server grants.
    pub allow: u8,
    /// Require the hybrid X25519 + ML-KEM-768 exchange (`lethed -C
    /// x25519mlkem768`): a `Tauth`, and a request on a ticket from one,
    /// fail with `post-quantum key exchange required`.
    pub pq: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            lifetime: 3600,
            window: 60,
            allow: rights::READ,
            pq: false,
        }
    }
}

/// What the server needs from its host for one frame.
pub struct Env<'a> {
    /// Seconds since 1970 UTC.
    pub now: u64,
    pub random: &'a mut dyn FnMut(&mut [u8]),
    /// The trust for a user name.
    pub trust: &'a mut dyn FnMut(&str) -> Trust,
}

/// What happened, for a log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A ticket was issued.
    Issued {
        uname: String,
        rights: u8,
        prefix: String,
    },
    /// A request was answered (perhaps with a sealed error).
    Answered {
        uname: String,
        op: &'static str,
        path: String,
        error: Option<String>,
    },
    /// A frame was refused in the clear.
    Refused(&'static str),
}

/// A server: its settings and ticket keys. The first key seals new
/// tickets; the others still open old ones.
pub struct Server {
    pub config: Config,
    pub keys: Vec<TicketKey>,
    /// Whether clear requests (`Tclear`) are answered: `lethed -A`. Off
    /// by default; they are then refused with `authentication required`.
    pub clear: bool,
}

/// Who a request is answered for.
struct Who<'a> {
    uname: &'a str,
    rights: u8,
    prefix: &'a str,
    /// Whether the user can own a file. A clear request never does: it
    /// gets the other bits whatever the tree says the owner is called.
    owner: bool,
}

impl Who<'_> {
    fn allowed(&self, d: &Dir, bit: u32) -> bool {
        if self.owner {
            allowed(self.uname, d, bit)
        } else {
            d.mode & bit != 0
        }
    }
}

/// The encoded `Rwalk` for walking `names` from the directory `path`,
/// whose entry is `d` and which the requester can reach. Each name is
/// checked as a walk of the joined path would be: the directory above it
/// must be one, and searchable. The first name that cannot be walked
/// ends the walk, and its error goes in the reply with the qids so far.
fn walk(tree: &dyn Tree, tk: &Who<'_>, path: &str, d: Dir, names: &[&str]) -> Vec<u8> {
    let mut qids = Vec::with_capacity(names.len());
    let mut cur = d;
    let mut p = String::from(path);
    for name in names {
        let step = || -> Result<Dir, String> {
            if cur.mode & DMDIR == 0 {
                return Err(String::from(ENOTDIR));
            }
            if !tk.allowed(&cur, 1) {
                return Err(String::from(EPERM));
            }
            let next = path::join(&p, name);
            if !path::valid_elem(name) || !path::valid(&next) {
                return Err(String::from(purg_abi::error::EFILENAME));
            }
            tree.stat(&next)
        };
        match step() {
            Ok(next) => {
                p = path::join(&p, name);
                qids.push(next.qid);
                cur = next;
            }
            Err(e) => {
                return Reply::Walk {
                    qids,
                    error: &e,
                    stat: None,
                }
                .encode();
            }
        }
    }
    Reply::Walk {
        qids,
        error: "",
        stat: Some(cur),
    }
    .encode()
}

/// Whether `user` may access `d` for `bit` (4 read, 1 search): the
/// owner's bits for its owner, else the other bits.
pub fn allowed(user: &str, d: &Dir, bit: u32) -> bool {
    let bits = if d.uid == user { d.mode >> 6 } else { d.mode };
    bits & bit != 0
}

impl Server {
    pub fn new(config: Config, key: [u8; 32]) -> Server {
        Server {
            config,
            keys: alloc::vec![TicketKey::new(key)],
            clear: false,
        }
    }

    /// The server with clear requests allowed or not.
    pub fn with_clear(mut self, clear: bool) -> Server {
        self.clear = clear;
        self
    }

    /// Answers one whole frame.
    pub fn handle(&self, tree: &dyn Tree, env: &mut Env<'_>, frame: &[u8]) -> (Vec<u8>, Event) {
        let id = msg::frame_id(frame);
        let refuse = |e: &'static str| (msg::rerror(id, e), Event::Refused(e));
        let m = match Msg::decode(frame) {
            Ok(m) => m,
            Err(e) => return refuse(e),
        };
        match m.frame {
            Frame::Tauth(_) if self.config.pq => refuse(PQ_REQUIRED),
            Frame::Tauth(t) => match self.auth(env, id, &t, None) {
                Ok(r) => r,
                Err(e) => refuse(e),
            },
            Frame::Tauth2 { t, ek } => match self.auth(env, id, &t, Some(ek)) {
                Ok(r) => r,
                Err(e) => refuse(e),
            },
            Frame::Treq {
                ticket,
                nonce,
                sealed,
            } => match self.request(tree, env, frame, id, ticket, &nonce, sealed) {
                Ok(r) => r,
                Err(e) => refuse(e),
            },
            // The clear mode is asked for on both ends: a server that
            // was not told to allow it says so and reads nothing.
            Frame::Tclear { .. } if !self.clear => refuse(REQUIRED),
            Frame::Tclear { request } => self.clear_request(tree, id, request),
            _ => refuse(MESSAGE),
        }
    }

    /// Answers a clear request as `none`, for reads anywhere in the tree
    /// that the other bits allow.
    fn clear_request(&self, tree: &dyn Tree, id: u32, request: &[u8]) -> (Vec<u8>, Event) {
        let who = Who {
            uname: NONE,
            rights: self.config.allow & rights::READ,
            prefix: "/",
            owner: false,
        };
        let (reply, op, p) = self.decoded(tree, &who, Op::decode(request).as_ref());
        let (bytes, error) = match reply {
            Ok(r) => (r, None),
            Err(e) => (Reply::Error(&e).encode(), Some(e)),
        };
        (
            msg::rclear(id, &bytes),
            Event::Answered {
                uname: String::from(NONE),
                op,
                path: p,
                error,
            },
        )
    }

    /// The reply to a request that decoded or did not, with the op's name
    /// and path for the log.
    fn decoded(
        &self,
        tree: &dyn Tree,
        who: &Who<'_>,
        op: Result<&Op<'_>, &Undecoded>,
    ) -> (Result<Vec<u8>, String>, &'static str, String) {
        match op {
            Ok(op) => (
                self.answer(tree, who, op),
                op.name(),
                String::from(op.path()),
            ),
            Err(Undecoded::Write(_)) => (Err(String::from(EPERM)), "write", String::new()),
            Err(Undecoded::Unsupported(_)) => {
                (Err(String::from(ENOTIMPL)), "unknown", String::new())
            }
            Err(Undecoded::Bad) => (Err(String::from(MESSAGE)), "bad", String::new()),
        }
    }

    fn auth(
        &self,
        env: &mut Env<'_>,
        id: u32,
        t: &Tauth<'_>,
        ek: Option<&[u8; KEM_PUBLIC]>,
    ) -> Result<(Vec<u8>, Event), &'static str> {
        if !path::valid(t.prefix) || !purg_auth::cert::valid_name(t.uname) {
            return Err(FAILED);
        }
        let trust = (env.trust)(t.uname);
        let policy = Policy {
            keys: &trust.keys,
            signers: &trust.signers,
            revoked: trust.revoked.as_deref(),
            now: env.now,
            hybrid: false,
        };
        // No key, nobody: refused as an export that trusts nothing
        // refuses (`docs/auth.md`, "Trust by default"). A server that
        // answers clear requests says so instead, as an export with `-A`
        // answers a `Tauth`; the client decides what to do, and ours
        // never falls back by itself.
        if !policy.requires_auth() {
            return Err(if self.clear { NOT_REQUIRED } else { NO_TRUST });
        }
        let signed = match ek {
            None => t.message(),
            Some(ek) => msg::auth2_message(&t.eph, ek, t.rights, t.prefix, t.uname, t.kind, t.body),
        };
        // Lethe's Tauth carries an Ed25519 signature, so only Ed25519 keys
        // and `pgcert1` certificates (`docs/lethe.md`, Section 6).
        if !matches!(t.kind, KIND_KEY | KIND_CERT) {
            return Err(FAILED);
        }
        if t.kind == KIND_KEY && policy.revoked.is_none() {
            return Err(purg_abi::auth::REVOKED);
        }
        check_signed(
            &policy,
            t.kind,
            &t.signature,
            t.body,
            &signed,
            Some(t.uname),
        )?;
        let granted = t.rights & self.config.allow & rights::GRANTABLE;
        if granted == 0 {
            return Err(EPERM);
        }
        let mut random = [0u8; 32];
        (env.random)(&mut random);
        let eph = Ephemeral::new(random);
        let shared = eph.shared(&t.eph).ok_or(FAILED)?;
        // The hybrid exchange: encapsulate to the client's ML-KEM key, and
        // mark the ticket as made by it.
        let kem = match ek {
            None => None,
            Some(ek) => {
                let mut m = [0u8; 32];
                (env.random)(&mut m);
                Some(purg_auth::kem::encapsulate(ek, &m).ok_or(FAILED)?)
            }
        };
        let master = match &kem {
            None => crypto::master(&shared, &crypto::transcript(&signed, &eph.public)),
            Some((ct, k)) => {
                crypto::master2(k, &shared, &crypto::transcript2(&signed, &eph.public, ct))
            }
        };
        let pq = if kem.is_some() { rights::PQ } else { 0 };
        let expires = env.now.saturating_add(self.config.lifetime);
        let tk = Ticket {
            master,
            issued: env.now,
            expires,
            rights: granted | pq,
            uname: String::from(t.uname),
            prefix: String::from(t.prefix),
        };
        let mut n = [0u8; NONCE];
        (env.random)(&mut n);
        let sealed = self.keys.first().ok_or(FAILED)?.seal(&tk, &n);
        let grant = Grant {
            now: env.now,
            expires,
            rights: granted,
            prefix: t.prefix,
            ticket: &sealed,
        }
        .encode();
        (env.random)(&mut n);
        let keys = Keys::from_master(&master);
        let reply = match &kem {
            None => crypto::rauth(&keys, id, &eph.public, &n, &grant),
            Some((ct, _)) => crypto::rauth2(&keys, id, &eph.public, ct, &n, &grant),
        };
        Ok((
            reply,
            Event::Issued {
                uname: tk.uname,
                rights: granted,
                prefix: tk.prefix,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn request(
        &self,
        tree: &dyn Tree,
        env: &mut Env<'_>,
        frame: &[u8],
        id: u32,
        ticket: &[u8],
        nonce: &[u8; NONCE],
        sealed: &[u8],
    ) -> Result<(Vec<u8>, Event), &'static str> {
        let tk = ticket::open(&self.keys, ticket)?;
        if env.now >= tk.expires {
            return Err(TICKET_EXPIRED);
        }
        if self.config.pq && tk.rights & rights::PQ == 0 {
            return Err(PQ_REQUIRED);
        }
        let keys = Keys::from_master(&tk.master);
        let plain = crypto::open_treq(&keys, frame, ticket, nonce, sealed)?;
        let decoded = Request::decode(&plain);
        let time = match &decoded {
            Ok(r) => Some(r.time),
            Err((t, _)) => *t,
        };
        if time.is_none_or(|t| t.abs_diff(env.now) > self.config.window) {
            return Err(REQUEST_EXPIRED);
        }
        let who = Who {
            uname: &tk.uname,
            rights: tk.rights,
            prefix: &tk.prefix,
            owner: true,
        };
        let op = decoded.as_ref().map(|r| &r.op).map_err(|(_, u)| u);
        let (reply, op, p) = self.decoded(tree, &who, op);
        let (bytes, error) = match reply {
            Ok(r) => (r, None),
            Err(e) => (Reply::Error(&e).encode(), Some(e)),
        };
        let mut n = [0u8; NONCE];
        (env.random)(&mut n);
        let out = crypto::rreq(&keys, id, nonce, &n, &bytes);
        Ok((
            out,
            Event::Answered {
                uname: tk.uname,
                op,
                path: p,
                error,
            },
        ))
    }

    /// Checks the requester's rights and prefix and the tree's permissions
    /// for `path`: search on every directory above it.
    fn reach(&self, tree: &dyn Tree, tk: &Who<'_>, p: &str) -> Result<Dir, String> {
        if !path::valid(p) {
            return Err(String::from(purg_abi::error::EFILENAME));
        }
        if tk.rights & rights::READ == 0 || !path::within(p, tk.prefix) {
            return Err(String::from(EPERM));
        }
        for a in path::ancestors(p) {
            let d = tree.stat(a)?;
            if d.mode & DMDIR == 0 {
                return Err(String::from(ENOTDIR));
            }
            if !tk.allowed(&d, 1) {
                return Err(String::from(EPERM));
            }
        }
        tree.stat(p)
    }

    /// The encoded reply to `op`, or its error.
    fn answer(&self, tree: &dyn Tree, tk: &Who<'_>, op: &Op<'_>) -> Result<Vec<u8>, String> {
        let d = self.reach(tree, tk, op.path())?;
        let mut data = Vec::new();
        match *op {
            Op::Stat { .. } => Ok(Reply::Stat(d).encode()),
            Op::Walk { path, ref names } => Ok(walk(tree, tk, path, d, names)),
            Op::Read {
                path,
                offset,
                count,
                have,
            } => {
                if d.mode & DMDIR != 0 {
                    return Err(String::from(EISDIR));
                }
                if !tk.allowed(&d, 4) {
                    return Err(String::from(EPERM));
                }
                if have == Some(d.qid.vers) {
                    return Ok(Reply::Read {
                        qid: d.qid,
                        unchanged: true,
                        data: &[],
                    }
                    .encode());
                }
                data.resize(count.min(MAX_COUNT) as usize, 0);
                let (qid, n) = tree.read(path, offset, &mut data)?;
                data.truncate(n.min(data.len()));
                Ok(Reply::Read {
                    qid,
                    unchanged: false,
                    data: &data,
                }
                .encode())
            }
            Op::Readdir {
                path,
                cookie,
                count,
            } => {
                if d.mode & DMDIR == 0 {
                    return Err(String::from(ENOTDIR));
                }
                if !tk.allowed(&d, 4) {
                    return Err(String::from(EPERM));
                }
                let max = count.min(MAX_COUNT) as usize;
                // No entry packs smaller than STATFIXLEN, so one more than
                // this many cannot fit: a full list always overflows the
                // page, and a shorter one is the end of the directory.
                let (qid, list) = tree.list(path, cookie, max / STATFIXLEN + 1)?;
                let mut last: &[u8] = &[];
                let mut more = false;
                for e in &list {
                    if data.len() + e.packed_len() > max {
                        more = true;
                        break;
                    }
                    e.pack_into(&mut data).map_err(|_| String::from(MESSAGE))?;
                    last = e.name.as_bytes();
                }
                if more && data.is_empty() {
                    return Err(String::from(ESHORTSTAT));
                }
                Ok(Reply::Readdir {
                    qid,
                    cookie: if more { last } else { &[] },
                    entries: &data,
                }
                .encode())
            }
        }
    }
}
