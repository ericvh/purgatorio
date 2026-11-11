//! The client's side of Lethe: the ticket exchange ([`auth`]), sealed
//! requests and their replies ([`Session`]), and [`Client`], which drives
//! both over a [`Transport`] and renews the ticket when the server asks.
//!
//! Nothing here keeps a connection: a [`Session`] is only the ticket and
//! its keys, so a request can go out on any connection, to any replica.

use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::auth::FAILED;
use purg_auth::cert::Cert;
use purg_auth::session::Ephemeral;

use crate::crypto::{self, Keys};
use crate::error::MESSAGE;
use crate::msg::{self, Frame, Grant, Msg, Op, Owned, Reply, Request, Tauth};
use crate::{NONCE, path};

/// A ticket is renewed this many seconds before it expires.
pub const RENEW_MARGIN: u64 = 5;

/// What a client signs with.
#[derive(Clone)]
pub struct Creds {
    pub user: String,
    pub seed: [u8; 32],
    /// A certificate for the seed's key, if the client has one.
    pub cert: Option<Cert>,
}

/// A ticket and what goes with it.
#[derive(Clone)]
pub struct Session {
    ticket: Vec<u8>,
    keys: Keys,
    /// The server's clock minus the client's, in seconds.
    skew: i64,
    /// Server time the ticket expires.
    pub expires: u64,
    pub rights: u8,
    pub prefix: String,
}

/// An exchange waiting for its `Rauth`.
pub struct AuthPending {
    id: u32,
    eph: Ephemeral,
    signed: Vec<u8>,
}

/// Starts the ticket exchange: the `Tauth` frame and what its reply needs.
/// `random` makes the ephemeral key.
pub fn auth(
    creds: &Creds,
    id: u32,
    rights: u8,
    prefix: &str,
    random: [u8; 32],
) -> (Vec<u8>, AuthPending) {
    let eph = Ephemeral::new(random);
    let public = purg_auth::public_key(&creds.seed);
    let cert = creds.cert.as_ref().filter(|c| c.key == public);
    let (kind, body) = purg_auth::proof::body(cert, &public);
    let signed = msg::auth_message(&eph.public, rights, prefix, &creds.user, kind, &body);
    let signature = purg_auth::sign(&creds.seed, &signed);
    let frame = Msg {
        id,
        frame: Frame::Tauth(Tauth {
            eph: eph.public,
            rights,
            prefix,
            uname: &creds.user,
            kind,
            body: &body,
            signature,
        }),
    }
    .encode();
    (frame, AuthPending { id, eph, signed })
}

impl AuthPending {
    /// The session from the server's reply. `local` is the client's clock,
    /// in seconds. An `Rerror` gives its string.
    pub fn finish(self, frame: &[u8], local: u64) -> Result<Session, String> {
        let m = Msg::decode(frame).map_err(String::from)?;
        if m.id != self.id {
            return Err(String::from(MESSAGE));
        }
        let (eph, nonce, sealed) = match m.frame {
            Frame::Rauth { eph, nonce, sealed } => (eph, nonce, sealed),
            Frame::Rerror { ename } => return Err(String::from(ename)),
            _ => return Err(String::from(MESSAGE)),
        };
        let shared = self.eph.shared(&eph).ok_or(FAILED)?;
        let master = crypto::master(&shared, &crypto::transcript(&self.signed, &eph));
        let keys = Keys::from_master(&master);
        let plain = crypto::open_rauth(&keys, frame, &eph, &nonce, sealed)?;
        let g = Grant::decode(&plain)?;
        if !path::valid(g.prefix) {
            return Err(String::from(MESSAGE));
        }
        Ok(Session {
            ticket: g.ticket.to_vec(),
            keys,
            skew: (g.now as i64).wrapping_sub(local as i64),
            expires: g.expires,
            rights: g.rights,
            prefix: String::from(g.prefix),
        })
    }
}

/// A request waiting for its reply.
pub struct ReqPending {
    id: u32,
    nonce: [u8; NONCE],
}

/// Why a request got no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// An error in the clear: the server could not open the ticket or the
    /// request ([`crate::error::renew`] says whether a new ticket helps).
    Clear(String),
    /// The server's answer, sealed: a file error.
    Sealed(String),
    /// The reply did not open or decode.
    Bad(&'static str),
}

impl Failure {
    pub fn text(&self) -> &str {
        match self {
            Failure::Clear(s) | Failure::Sealed(s) => s,
            Failure::Bad(s) => s,
        }
    }
}

impl Session {
    /// The client's estimate of the server's clock.
    pub fn server_time(&self, local: u64) -> u64 {
        (local as i64).wrapping_add(self.skew).max(0) as u64
    }

    /// Whether the ticket should be renewed before the next request.
    pub fn expiring(&self, local: u64) -> bool {
        self.server_time(local).saturating_add(RENEW_MARGIN) >= self.expires
    }

    /// The ticket, opaque.
    pub fn ticket(&self) -> &[u8] {
        &self.ticket
    }

    /// A `Treq` for `op`, timed by `local`, sealed with `nonce` (random).
    pub fn request(
        &self,
        id: u32,
        local: u64,
        op: &Op<'_>,
        nonce: [u8; NONCE],
    ) -> (Vec<u8>, ReqPending) {
        let plain = Request {
            time: self.server_time(local),
            op: op.clone(),
        }
        .encode();
        self.seal(id, &plain, nonce)
    }

    /// A `Treq` carrying `plain`, an encoded request, as it is: for ops
    /// this version has no type for (writes, streams) and for tests.
    pub fn seal(&self, id: u32, plain: &[u8], nonce: [u8; NONCE]) -> (Vec<u8>, ReqPending) {
        (
            crypto::treq(&self.keys, id, &self.ticket, &nonce, plain),
            ReqPending { id, nonce },
        )
    }

    /// Opens the reply to `p`.
    pub fn reply(&self, p: &ReqPending, frame: &[u8]) -> Result<Owned, Failure> {
        let m = Msg::decode(frame).map_err(Failure::Bad)?;
        if m.id != p.id {
            return Err(Failure::Bad(MESSAGE));
        }
        let (nonce, sealed) = match m.frame {
            Frame::Rreq { nonce, sealed } => (nonce, sealed),
            Frame::Rerror { ename } => return Err(Failure::Clear(String::from(ename))),
            _ => return Err(Failure::Bad(MESSAGE)),
        };
        let plain =
            crypto::open_rreq(&self.keys, frame, &p.nonce, &nonce, sealed).map_err(Failure::Bad)?;
        Reply::decode(&plain)
            .map_err(Failure::Bad)?
            .owned()
            .map_err(Failure::Sealed)
    }
}

/// Moves one frame to the server and its reply back.
pub trait Transport {
    fn exchange(&mut self, frame: &[u8]) -> Result<Vec<u8>, String>;
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The transport failed: the caller may reconnect and call again.
    Transport(String),
    /// The server's answer.
    Remote(String),
}

/// A client: credentials, what it asks for, and its current ticket.
pub struct Client {
    pub creds: Creds,
    pub rights: u8,
    pub prefix: String,
    pub session: Option<Session>,
    /// Tickets obtained, renewals included.
    pub tickets: u32,
    next: u32,
}

impl Client {
    pub fn new(creds: Creds, rights: u8, prefix: &str) -> Client {
        Client {
            creds,
            rights,
            prefix: String::from(prefix),
            session: None,
            tickets: 0,
            next: 1,
        }
    }

    fn id(&mut self) -> u32 {
        self.next = self.next.wrapping_add(1).max(1);
        self.next
    }

    /// Gets a ticket over `t`.
    pub fn authenticate(
        &mut self,
        t: &mut dyn Transport,
        local: u64,
        random: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), CallError> {
        let mut r = [0u8; 32];
        random(&mut r);
        let id = self.id();
        let (frame, pending) = auth(&self.creds, id, self.rights, &self.prefix, r);
        let reply = t.exchange(&frame).map_err(CallError::Transport)?;
        self.session = Some(pending.finish(&reply, local).map_err(CallError::Remote)?);
        self.tickets += 1;
        Ok(())
    }

    /// Sends `op`, getting or renewing the ticket as needed. `now` reads
    /// the client's clock in seconds.
    pub fn call(
        &mut self,
        t: &mut dyn Transport,
        op: &Op<'_>,
        now: &dyn Fn() -> u64,
        random: &mut dyn FnMut(&mut [u8]),
    ) -> Result<Owned, CallError> {
        for attempt in 0..2 {
            if self.session.as_ref().is_none_or(|s| s.expiring(now())) {
                self.authenticate(t, now(), random)?;
            }
            let id = self.id();
            let Some(s) = &self.session else {
                return Err(CallError::Remote(String::from(FAILED)));
            };
            let mut n = [0u8; NONCE];
            random(&mut n);
            let (frame, pending) = s.request(id, now(), op, n);
            let reply = t.exchange(&frame).map_err(CallError::Transport)?;
            match s.reply(&pending, &reply) {
                Ok(o) => return Ok(o),
                Err(Failure::Clear(e)) if attempt == 0 && crate::error::renew(&e) => {
                    self.session = None;
                }
                Err(f) => return Err(CallError::Remote(String::from(f.text()))),
            }
        }
        Err(CallError::Remote(String::from(crate::error::TICKET)))
    }
}

/// Reads one frame with `read_exact`, checking its size before the body.
pub fn read_frame<E: From<&'static str>>(
    read_exact: &mut dyn FnMut(&mut [u8]) -> Result<(), E>,
) -> Result<Vec<u8>, E> {
    let mut size = [0u8; 4];
    read_exact(&mut size)?;
    let n = msg::frame_size(size)?;
    let mut v = alloc::vec![0u8; n];
    v[..4].copy_from_slice(&size);
    read_exact(&mut v[4..])?;
    Ok(v)
}
