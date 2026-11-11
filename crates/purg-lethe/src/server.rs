//! A Lethe server: one frame in, one frame out, nothing kept
//! (`docs/lethe.md`). [`Server::handle`] answers a frame from a
//! [`Tree`], the trust files and the clock; the caller moves frames on and
//! off the wire.

use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::auth::{FAILED, KIND_CERT, KIND_KEY};
use purg_abi::error::{EISDIR, ENOTDIR, ENOTIMPL, EPERM, ESHORTSTAT};
use purg_auth::cert::Cert;
use purg_auth::proof::{Policy, check_cert};
use purg_auth::session::Ephemeral;
use purg_styx::{DMDIR, Dir, Qid};

use crate::crypto::{self, Keys};
use crate::error::{MESSAGE, REQUEST_EXPIRED, TICKET_EXPIRED};
use crate::msg::{self, Frame, Grant, Msg, Op, Reply, Request, Tauth, Undecoded};
use crate::ticket::{self, Ticket, TicketKey};
use crate::{MAX_COUNT, NONCE, path, rights};

/// The tree a server serves. Paths are valid Lethe paths. Errors are
/// Inferno strings. Permission checks are the server's, from the modes
/// and owners the tree reports.
pub trait Tree {
    fn stat(&self, path: &str) -> Result<Dir, String>;
    /// Reads `path` at `offset`; returns the file's qid at the time of
    /// the read and the count.
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(Qid, usize), String>;
    /// The directory's qid and its entries, sorted by name (as bytes).
    fn list(&self, path: &str) -> Result<(Qid, Vec<Dir>), String>;
}

/// The keys a server trusts for one user, read when a `Tauth` comes in.
#[derive(Clone, Default, Debug)]
pub struct Trust {
    /// Keys held directly.
    pub keys: Vec<[u8; 32]>,
    pub signers: Vec<[u8; 32]>,
    /// `None` when the list is damaged: every certificate then fails.
    pub revoked: Option<Vec<[u8; 32]>>,
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
}

impl Default for Config {
    fn default() -> Config {
        Config {
            lifetime: 3600,
            window: 60,
            allow: rights::READ,
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
        }
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
            Frame::Tauth(t) => match self.auth(env, id, &t) {
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
            _ => refuse(MESSAGE),
        }
    }

    fn auth(
        &self,
        env: &mut Env<'_>,
        id: u32,
        t: &Tauth<'_>,
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
        };
        // There is no unauthenticated mode: no key, nobody.
        if !policy.requires_auth() {
            return Err(FAILED);
        }
        let signed = t.message();
        match t.kind {
            KIND_KEY => {
                let key: [u8; 32] = t.body.try_into().map_err(|_| FAILED)?;
                if !policy.keys.contains(&key) || !purg_auth::verify(&key, &signed, &t.signature) {
                    return Err(FAILED);
                }
                if policy.revoked.is_none_or(|r| r.contains(&key)) {
                    return Err(purg_abi::auth::REVOKED);
                }
            }
            KIND_CERT => {
                let cert = Cert::parse(t.body).map_err(|_| FAILED)?;
                if !purg_auth::verify(&cert.key, &signed, &t.signature) {
                    return Err(FAILED);
                }
                check_cert(&policy, &cert, Some(t.uname))?;
            }
            _ => return Err(FAILED),
        }
        let granted = t.rights & self.config.allow & rights::GRANTABLE;
        if granted == 0 {
            return Err(EPERM);
        }
        let mut random = [0u8; 32];
        (env.random)(&mut random);
        let eph = Ephemeral::new(random);
        let shared = eph.shared(&t.eph).ok_or(FAILED)?;
        let master = crypto::master(&shared, &crypto::transcript(&signed, &eph.public));
        let expires = env.now.saturating_add(self.config.lifetime);
        let tk = Ticket {
            master,
            issued: env.now,
            expires,
            rights: granted,
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
        let reply = crypto::rauth(&keys, id, &eph.public, &n, &grant);
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
        let (reply, op, p) = match &decoded {
            Ok(req) => (
                self.answer(tree, &tk, &req.op),
                req.op.name(),
                String::from(req.op.path()),
            ),
            Err((_, Undecoded::Write(_))) => (Err(String::from(EPERM)), "write", String::new()),
            Err((_, Undecoded::Unsupported(_))) => {
                (Err(String::from(ENOTIMPL)), "unknown", String::new())
            }
            Err((_, Undecoded::Bad)) => (Err(String::from(MESSAGE)), "bad", String::new()),
        };
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

    /// Checks the ticket's rights and prefix and the tree's permissions
    /// for `path`: search on every directory above it.
    fn reach(&self, tree: &dyn Tree, tk: &Ticket, p: &str) -> Result<Dir, String> {
        if !path::valid(p) {
            return Err(String::from(purg_abi::error::EFILENAME));
        }
        if tk.rights & rights::READ == 0 || !path::within(p, &tk.prefix) {
            return Err(String::from(EPERM));
        }
        for a in path::ancestors(p) {
            let d = tree.stat(a)?;
            if d.mode & DMDIR == 0 {
                return Err(String::from(ENOTDIR));
            }
            if !allowed(&tk.uname, &d, 1) {
                return Err(String::from(EPERM));
            }
        }
        tree.stat(p)
    }

    /// The encoded reply to `op`, or its error.
    fn answer(&self, tree: &dyn Tree, tk: &Ticket, op: &Op<'_>) -> Result<Vec<u8>, String> {
        let d = self.reach(tree, tk, op.path())?;
        let mut data = Vec::new();
        match *op {
            Op::Stat { .. } => Ok(Reply::Stat(d).encode()),
            Op::Read {
                path,
                offset,
                count,
                have,
            } => {
                if d.mode & DMDIR != 0 {
                    return Err(String::from(EISDIR));
                }
                if !allowed(&tk.uname, &d, 4) {
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
                if !allowed(&tk.uname, &d, 4) {
                    return Err(String::from(EPERM));
                }
                let (qid, list) = tree.list(path)?;
                let max = count.min(MAX_COUNT) as usize;
                let mut last: &[u8] = &[];
                let mut more = false;
                for e in list.iter().filter(|e| e.name.as_bytes() > cookie) {
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
