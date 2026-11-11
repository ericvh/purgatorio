//! Frames (`docs/lethe.md`, Sections 3 and 4) and the plaintexts sealed
//! inside them. Decoding is strict: a message must end exactly where its
//! last field does, so each has one encoding, and [`Msg::encode`] gives
//! back the bytes [`Msg::decode`] took.

use alloc::string::String;
use alloc::vec::Vec;

use purg_styx::{Dir, Qid};

use crate::error::{MESSAGE, VERSION as EVERSION};
use crate::wire::{Reader, Writer};
use crate::{EPH, HEADER, MAX_COUNT, MAX_FRAME, NONCE, TAG, VERSION, kind, op};

/// The context string of a `Tauth` signature.
pub const AUTH_CONTEXT: &[u8] = b"lethe1 auth\0";

/// The size of the frame whose first four bytes are `size`, checked
/// before anything else is read.
pub fn frame_size(size: [u8; 4]) -> Result<usize, &'static str> {
    let n = u32::from_le_bytes(size) as usize;
    if (HEADER..=MAX_FRAME).contains(&n) {
        Ok(n)
    } else {
        Err(MESSAGE)
    }
}

/// A frame's header, as the associated data of what it seals.
pub fn header(ty: u8, id: u32, size: usize) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[..4].copy_from_slice(&(size as u32).to_le_bytes());
    h[4] = VERSION;
    h[5] = ty;
    h[6..].copy_from_slice(&id.to_le_bytes());
    h
}

/// The id of a frame, when it has a header at all.
pub fn frame_id(buf: &[u8]) -> u32 {
    buf.get(6..HEADER)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// `Tauth`: the client's ephemeral key, what it asks for, who it is, and
/// its signature over [`Tauth::message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tauth<'a> {
    pub eph: [u8; EPH],
    pub rights: u8,
    pub prefix: &'a str,
    pub uname: &'a str,
    /// `purg_abi::auth::KIND_KEY` or `KIND_CERT`.
    pub kind: u8,
    /// A public key or a `pgcert1` line.
    pub body: &'a [u8],
    pub signature: [u8; 64],
}

impl Tauth<'_> {
    /// What the signature covers.
    pub fn message(&self) -> Vec<u8> {
        auth_message(
            &self.eph,
            self.rights,
            self.prefix,
            self.uname,
            self.kind,
            self.body,
        )
    }
}

/// The signed message of a `Tauth` with these fields.
pub fn auth_message(
    eph: &[u8; EPH],
    rights: u8,
    prefix: &str,
    uname: &str,
    kind: u8,
    body: &[u8],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(AUTH_CONTEXT.len() + EPH + 8 + prefix.len() + body.len() + 64);
    let mut w = Writer { buf: &mut m };
    w.bytes(AUTH_CONTEXT);
    w.bytes(eph);
    w.u8(rights);
    w.data16(prefix.as_bytes());
    w.data16(uname.as_bytes());
    w.u8(kind);
    w.data16(body);
    m
}

/// An outer frame's body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Tauth(Tauth<'a>),
    /// The server's ephemeral key and the sealed [`Grant`].
    Rauth {
        eph: [u8; EPH],
        nonce: [u8; NONCE],
        sealed: &'a [u8],
    },
    /// A ticket and a sealed [`Request`].
    Treq {
        ticket: &'a [u8],
        nonce: [u8; NONCE],
        sealed: &'a [u8],
    },
    /// A sealed [`Reply`].
    Rreq {
        nonce: [u8; NONCE],
        sealed: &'a [u8],
    },
    Rerror {
        ename: &'a str,
    },
    /// A request in the clear: an encoded [`Op`], with no ticket, no time
    /// and no seal.
    Tclear {
        request: &'a [u8],
    },
    /// A reply in the clear: an encoded [`Reply`].
    Rclear {
        reply: &'a [u8],
    },
}

/// A whole frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Msg<'a> {
    pub id: u32,
    pub frame: Frame<'a>,
}

fn sealed<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], &'static str> {
    let s = r.rest();
    if s.len() < TAG {
        return Err(MESSAGE);
    }
    Ok(s)
}

impl<'a> Msg<'a> {
    /// Decodes one whole frame.
    pub fn decode(buf: &'a [u8]) -> Result<Msg<'a>, &'static str> {
        let mut r = Reader::new(buf);
        let size = r.u32()? as usize;
        if size != buf.len() || size > MAX_FRAME {
            return Err(MESSAGE);
        }
        if r.u8()? != VERSION {
            return Err(EVERSION);
        }
        let ty = r.u8()?;
        let id = r.u32()?;
        let frame = match ty {
            kind::TAUTH => {
                let t = Tauth {
                    eph: r.array()?,
                    rights: r.u8()?,
                    prefix: r.str16()?,
                    uname: r.str16()?,
                    kind: r.u8()?,
                    body: r.data16()?,
                    signature: r.array()?,
                };
                Frame::Tauth(t)
            }
            kind::RAUTH => Frame::Rauth {
                eph: r.array()?,
                nonce: r.array()?,
                sealed: sealed(&mut r)?,
            },
            kind::TREQ => Frame::Treq {
                ticket: r.data16()?,
                nonce: r.array()?,
                sealed: sealed(&mut r)?,
            },
            kind::RREQ => Frame::Rreq {
                nonce: r.array()?,
                sealed: sealed(&mut r)?,
            },
            kind::RERROR => Frame::Rerror { ename: r.str16()? },
            kind::TCLEAR => Frame::Tclear { request: r.rest() },
            kind::RCLEAR => Frame::Rclear { reply: r.rest() },
            _ => return Err(MESSAGE),
        };
        r.finish()?;
        Ok(Msg { id, frame })
    }

    /// The type byte of this frame.
    pub fn ty(&self) -> u8 {
        match self.frame {
            Frame::Tauth(_) => kind::TAUTH,
            Frame::Rauth { .. } => kind::RAUTH,
            Frame::Treq { .. } => kind::TREQ,
            Frame::Rreq { .. } => kind::RREQ,
            Frame::Rerror { .. } => kind::RERROR,
            Frame::Tclear { .. } => kind::TCLEAR,
            Frame::Rclear { .. } => kind::RCLEAR,
        }
    }

    /// The frame's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let mut w = Writer { buf: &mut body };
        match &self.frame {
            Frame::Tauth(t) => {
                w.bytes(&t.eph);
                w.u8(t.rights);
                w.data16(t.prefix.as_bytes());
                w.data16(t.uname.as_bytes());
                w.u8(t.kind);
                w.data16(t.body);
                w.bytes(&t.signature);
            }
            Frame::Rauth { eph, nonce, sealed } => {
                w.bytes(eph);
                w.bytes(nonce);
                w.bytes(sealed);
            }
            Frame::Treq {
                ticket,
                nonce,
                sealed,
            } => {
                w.data16(ticket);
                w.bytes(nonce);
                w.bytes(sealed);
            }
            Frame::Rreq { nonce, sealed } => {
                w.bytes(nonce);
                w.bytes(sealed);
            }
            Frame::Rerror { ename } => w.data16(ename.as_bytes()),
            Frame::Tclear { request: b } | Frame::Rclear { reply: b } => w.bytes(b),
        }
        let mut out = Vec::with_capacity(HEADER + body.len());
        out.extend_from_slice(&header(self.ty(), self.id, HEADER + body.len()));
        out.extend_from_slice(&body);
        out
    }
}

/// A `Tclear` frame for `op`.
pub fn tclear(id: u32, op: &Op<'_>) -> Vec<u8> {
    Msg {
        id,
        frame: Frame::Tclear {
            request: &op.encode(),
        },
    }
    .encode()
}

/// An `Rclear` frame carrying `reply`, an encoded [`Reply`].
pub fn rclear(id: u32, reply: &[u8]) -> Vec<u8> {
    Msg {
        id,
        frame: Frame::Rclear { reply },
    }
    .encode()
}

/// A clear `Rerror` frame.
pub fn rerror(id: u32, ename: &str) -> Vec<u8> {
    Msg {
        id,
        frame: Frame::Rerror { ename },
    }
    .encode()
}

/// What `Rauth` seals: the server's clock, the ticket's expiry, what was
/// granted and the ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant<'a> {
    pub now: u64,
    pub expires: u64,
    pub rights: u8,
    pub prefix: &'a str,
    pub ticket: &'a [u8],
}

impl<'a> Grant<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        let mut w = Writer { buf: &mut v };
        w.u64(self.now);
        w.u64(self.expires);
        w.u8(self.rights);
        w.data16(self.prefix.as_bytes());
        w.data16(self.ticket);
        v
    }

    pub fn decode(buf: &'a [u8]) -> Result<Grant<'a>, &'static str> {
        let mut r = Reader::new(buf);
        let g = Grant {
            now: r.u64()?,
            expires: r.u64()?,
            rights: r.u8()?,
            prefix: r.str16()?,
            ticket: r.data16()?,
        };
        r.finish()?;
        Ok(g)
    }
}

/// An operation: inside a `Treq`, or the body of a `Tclear`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op<'a> {
    Stat {
        path: &'a str,
    },
    /// `have` is the version the client holds data for: `cond` 1.
    Read {
        path: &'a str,
        offset: u64,
        count: u32,
        have: Option<u32>,
    },
    Readdir {
        path: &'a str,
        cookie: &'a [u8],
        count: u32,
    },
}

impl Op<'_> {
    pub fn path(&self) -> &str {
        match self {
            Op::Stat { path } | Op::Read { path, .. } | Op::Readdir { path, .. } => path,
        }
    }

    /// The op's name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Stat { .. } => "stat",
            Op::Read { .. } => "read",
            Op::Readdir { .. } => "readdir",
        }
    }
}

/// A request that does not decode: malformed, or a code this version
/// reserves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Undecoded {
    Bad,
    /// A reserved write op.
    Write(u8),
    /// Another reserved or unknown op.
    Unsupported(u8),
}

/// The plaintext of a `Treq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    /// The client's estimate of the server's clock, in seconds.
    pub time: u64,
    pub op: Op<'a>,
}

impl<'a> Op<'a> {
    fn put(&self, w: &mut Writer<'_>) {
        match self {
            Op::Stat { path } => {
                w.u8(op::TSTAT);
                w.data16(path.as_bytes());
            }
            Op::Read {
                path,
                offset,
                count,
                have,
            } => {
                w.u8(op::TREAD);
                w.data16(path.as_bytes());
                w.u64(*offset);
                w.u32(*count);
                w.u8(have.is_some() as u8);
                w.u32(have.unwrap_or(0));
            }
            Op::Readdir {
                path,
                cookie,
                count,
            } => {
                w.u8(op::TREADDIR);
                w.data16(path.as_bytes());
                w.data16(cookie);
                w.u32(*count);
            }
        }
    }

    fn get(r: &mut Reader<'a>) -> Result<Op<'a>, Undecoded> {
        let bad = |_| Undecoded::Bad;
        let code = r.u8().map_err(bad)?;
        let op = match code {
            op::TSTAT => Op::Stat {
                path: r.str16().map_err(bad)?,
            },
            op::TREAD => {
                let path = r.str16().map_err(bad)?;
                let offset = r.u64().map_err(bad)?;
                let count = r.u32().map_err(bad)?;
                let cond = r.u8().map_err(bad)?;
                let vers = r.u32().map_err(bad)?;
                let have = match (cond, vers) {
                    (0, 0) => None,
                    (1, v) => Some(v),
                    _ => return Err(Undecoded::Bad),
                };
                Op::Read {
                    path,
                    offset,
                    count,
                    have,
                }
            }
            op::TREADDIR => Op::Readdir {
                path: r.str16().map_err(bad)?,
                cookie: r.data16().map_err(bad)?,
                count: r.u32().map_err(bad)?,
            },
            c if op::WRITES.contains(&c) => return Err(Undecoded::Write(c)),
            c => return Err(Undecoded::Unsupported(c)),
        };
        r.finish().map_err(bad)?;
        Ok(op)
    }

    /// `op u8 fields`: the body of a `Tclear`, and a `Treq`'s plaintext
    /// after its time.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.put(&mut Writer { buf: &mut v });
        v
    }

    /// Decodes what [`Op::encode`] made, strictly.
    pub fn decode(buf: &'a [u8]) -> Result<Op<'a>, Undecoded> {
        Op::get(&mut Reader::new(buf))
    }
}

impl<'a> Request<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        let mut w = Writer { buf: &mut v };
        w.u64(self.time);
        self.op.put(&mut w);
        v
    }

    /// Decodes a request. The time is returned even for a request whose
    /// op does not decode, so the server can check it first.
    pub fn decode(buf: &'a [u8]) -> Result<Request<'a>, (Option<u64>, Undecoded)> {
        let mut r = Reader::new(buf);
        let time = r.u64().map_err(|_| (None, Undecoded::Bad))?;
        let op = Op::get(&mut r).map_err(|u| (Some(time), u))?;
        Ok(Request { time, op })
    }
}

/// The plaintext of an `Rreq`, and the body of an `Rclear`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    Stat(Dir),
    Read {
        qid: Qid,
        unchanged: bool,
        data: &'a [u8],
    },
    /// `entries` are packed Styx stat entries.
    Readdir {
        qid: Qid,
        cookie: &'a [u8],
        entries: &'a [u8],
    },
    Error(&'a str),
}

fn put_qid(w: &mut Writer<'_>, q: &Qid) {
    w.u8(q.qtype);
    w.u32(q.vers);
    w.u64(q.path);
}

fn get_qid(r: &mut Reader<'_>) -> Result<Qid, &'static str> {
    let qtype = r.u8()?;
    let vers = r.u32()?;
    let path = r.u64()?;
    Ok(Qid { path, vers, qtype })
}

impl<'a> Reply<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        let mut w = Writer { buf: &mut v };
        match self {
            // A Dir from a tree has short names; one that does not pack
            // goes out as an error instead.
            Reply::Stat(d) => match d.pack() {
                Ok(p) => {
                    w.u8(op::RSTAT);
                    w.bytes(&p);
                }
                Err(_) => return Reply::Error(MESSAGE).encode(),
            },
            Reply::Read {
                qid,
                unchanged,
                data,
            } => {
                w.u8(op::RREAD);
                put_qid(&mut w, qid);
                w.u8(*unchanged as u8);
                w.data32(data);
            }
            Reply::Readdir {
                qid,
                cookie,
                entries,
            } => {
                w.u8(op::RREADDIR);
                put_qid(&mut w, qid);
                w.data16(cookie);
                w.data32(entries);
            }
            Reply::Error(e) => {
                w.u8(op::RERROR);
                w.data16(e.as_bytes());
            }
        }
        v
    }

    pub fn decode(buf: &'a [u8]) -> Result<Reply<'a>, &'static str> {
        let mut r = Reader::new(buf);
        let reply = match r.u8()? {
            op::RSTAT => {
                let rest = r.rest();
                let (d, n) = Dir::unpack(rest).map_err(|_| MESSAGE)?;
                if n != rest.len() {
                    return Err(MESSAGE);
                }
                Reply::Stat(d)
            }
            op::RREAD => {
                let qid = get_qid(&mut r)?;
                let unchanged = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(MESSAGE),
                };
                let data = r.data32()?;
                if data.len() > MAX_COUNT as usize || (unchanged && !data.is_empty()) {
                    return Err(MESSAGE);
                }
                Reply::Read {
                    qid,
                    unchanged,
                    data,
                }
            }
            op::RREADDIR => {
                let qid = get_qid(&mut r)?;
                let cookie = r.data16()?;
                let entries = r.data32()?;
                if purg_styx::dirs(entries).any(|d| d.is_err()) {
                    return Err(MESSAGE);
                }
                Reply::Readdir {
                    qid,
                    cookie,
                    entries,
                }
            }
            op::RERROR => Reply::Error(r.str16()?),
            _ => return Err(MESSAGE),
        };
        r.finish()?;
        Ok(reply)
    }
}

/// A reply the client keeps after the frame is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owned {
    Stat(Dir),
    Read {
        qid: Qid,
        unchanged: bool,
        data: Vec<u8>,
    },
    Readdir {
        qid: Qid,
        cookie: Vec<u8>,
        entries: Vec<Dir>,
    },
}

impl Reply<'_> {
    /// The reply as owned data, or its error.
    pub fn owned(&self) -> Result<Owned, String> {
        Ok(match self {
            Reply::Stat(d) => Owned::Stat(d.clone()),
            Reply::Read {
                qid,
                unchanged,
                data,
            } => Owned::Read {
                qid: *qid,
                unchanged: *unchanged,
                data: data.to_vec(),
            },
            Reply::Readdir {
                qid,
                cookie,
                entries,
            } => Owned::Readdir {
                qid: *qid,
                cookie: cookie.to_vec(),
                entries: purg_styx::dirs(entries)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| String::from(MESSAGE))?,
            },
            Reply::Error(e) => return Err(String::from(*e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(m: &Msg<'_>) -> Vec<u8> {
        let b = m.encode();
        assert_eq!(&Msg::decode(&b).unwrap(), m);
        b
    }

    /// Fixed encodings: the layout of `docs/lethe.md` byte for byte.
    #[test]
    fn vectors() {
        let e = round(&Msg {
            id: 0x0403_0201,
            frame: Frame::Rerror {
                ename: "file does not exist",
            },
        });
        let mut want = alloc::vec![31, 0, 0, 0, 1, 5, 1, 2, 3, 4, 19, 0];
        want.extend_from_slice(b"file does not exist");
        assert_eq!(e, want);

        let r = Request {
            time: 0x0102,
            op: Op::Read {
                path: "/a",
                offset: 7,
                count: 8192,
                have: Some(9),
            },
        }
        .encode();
        assert_eq!(
            r,
            [
                2, 1, 0, 0, 0, 0, 0, 0, // time
                0x12, 2, 0, b'/', b'a', // op, path
                7, 0, 0, 0, 0, 0, 0, 0, // offset
                0, 0x20, 0, 0, // count
                1, 9, 0, 0, 0, // cond, vers
            ]
        );
        assert_eq!(
            Request::decode(&r).unwrap().op,
            Op::Read {
                path: "/a",
                offset: 7,
                count: 8192,
                have: Some(9)
            }
        );

        let q = Qid {
            path: 5,
            vers: 6,
            qtype: 0x80,
        };
        let d = Reply::Readdir {
            qid: q,
            cookie: b"z",
            entries: &[],
        }
        .encode();
        assert_eq!(
            d,
            [
                0x15, 0x80, 6, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, // op, qid
                1, 0, b'z', // cookie
                0, 0, 0, 0, // entries
            ]
        );
    }

    /// The clear form, byte for byte: a plain header and the op.
    #[test]
    fn clear_vectors() {
        let op = Op::Read {
            path: "/a",
            offset: 7,
            count: 8192,
            have: Some(9),
        };
        let t = tclear(0x0403_0201, &op);
        assert_eq!(
            t,
            [
                32, 0, 0, 0, 1, 9, 1, 2, 3, 4, // size, version, Tclear, id
                0x12, 2, 0, b'/', b'a', // op, path
                7, 0, 0, 0, 0, 0, 0, 0, // offset
                0, 0x20, 0, 0, // count
                1, 9, 0, 0, 0, // cond, vers
            ]
        );
        let m = Msg::decode(&t).unwrap();
        let Frame::Tclear { request } = m.frame else {
            panic!()
        };
        assert_eq!(Op::decode(request), Ok(op.clone()));
        // The same op in a sealed request's plaintext, after the time.
        assert_eq!(Request { time: 1, op }.encode()[8..], *request);
        let s = tclear(5, &Op::Stat { path: "/" });
        assert_eq!(s, [14, 0, 0, 0, 1, 9, 5, 0, 0, 0, 0x10, 1, 0, b'/']);
        let d = tclear(
            6,
            &Op::Readdir {
                path: "/d",
                cookie: b"k",
                count: 256,
            },
        );
        assert_eq!(
            d,
            [
                22, 0, 0, 0, 1, 9, 6, 0, 0, 0, // header
                0x14, 2, 0, b'/', b'd', 1, 0, b'k', 0, 1, 0, 0,
            ]
        );
        let q = Qid {
            path: 5,
            vers: 6,
            qtype: 0,
        };
        let reply = Reply::Read {
            qid: q,
            unchanged: false,
            data: b"hi",
        }
        .encode();
        let r = round(&Msg {
            id: 0x0403_0201,
            frame: Frame::Rclear { reply: &reply },
        });
        assert_eq!(r, rclear(0x0403_0201, &reply));
        assert_eq!(
            r,
            [
                31, 0, 0, 0, 1, 10, 1, 2, 3, 4, // size, version, Rclear, id
                0x13, 0, 6, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, // op, qid
                0, 2, 0, 0, 0, b'h', b'i', // unchanged, data
            ]
        );
        let e = rclear(1, &Reply::Error("permission denied").encode());
        let mut want = alloc::vec![30, 0, 0, 0, 1, 10, 1, 0, 0, 0, 0x7f, 17, 0];
        want.extend_from_slice(b"permission denied");
        assert_eq!(e, want);
        // Strict: a trailing byte, a bad cond, reserved ops.
        let mut long = Op::Stat { path: "/" }.encode();
        long.push(0);
        assert_eq!(Op::decode(&long), Err(Undecoded::Bad));
        assert_eq!(Op::decode(&[]), Err(Undecoded::Bad));
        assert_eq!(Op::decode(&[0x21]), Err(Undecoded::Write(0x21)));
        assert_eq!(Op::decode(&[0x16]), Err(Undecoded::Unsupported(0x16)));
    }

    #[test]
    fn frames_round_trip() {
        let t = Tauth {
            eph: [1; EPH],
            rights: 1,
            prefix: "/",
            uname: "glenda",
            kind: 0,
            body: &[2; 32],
            signature: [3; 64],
        };
        let b = round(&Msg {
            id: 9,
            frame: Frame::Tauth(t.clone()),
        });
        assert_eq!(frame_size(b[..4].try_into().unwrap()), Ok(b.len()));
        assert!(t.message().starts_with(AUTH_CONTEXT));
        round(&Msg {
            id: 1,
            frame: Frame::Treq {
                ticket: &[4; 80],
                nonce: [5; NONCE],
                sealed: &[6; 40],
            },
        });
        round(&Msg {
            id: 1,
            frame: Frame::Rreq {
                nonce: [5; NONCE],
                sealed: &[6; TAG],
            },
        });
        round(&Msg {
            id: 2,
            frame: Frame::Rauth {
                eph: [7; EPH],
                nonce: [5; NONCE],
                sealed: &[6; 30],
            },
        });
        let g = Grant {
            now: 1,
            expires: 2,
            rights: 1,
            prefix: "/lib",
            ticket: &[9; 10],
        };
        assert_eq!(Grant::decode(&g.encode()).unwrap(), g);
    }

    #[test]
    fn strict() {
        let b = rerror(1, "x");
        // A byte too many, a byte too few, another version, another type.
        let mut long = b.clone();
        long.push(0);
        long[0] += 1;
        assert_eq!(Msg::decode(&long), Err(MESSAGE));
        assert_eq!(Msg::decode(&b[..b.len() - 1]), Err(MESSAGE));
        let mut v = b.clone();
        v[4] = 2;
        assert_eq!(Msg::decode(&v), Err(EVERSION));
        let mut t = b.clone();
        t[5] = 6;
        assert_eq!(Msg::decode(&t), Err(MESSAGE));
        assert_eq!(frame_size([1, 0, 1, 0]), Err(MESSAGE));
        assert_eq!(frame_size([9, 0, 0, 0]), Err(MESSAGE));
        // A sealed body shorter than a tag.
        let short = Msg {
            id: 1,
            frame: Frame::Rreq {
                nonce: [0; NONCE],
                sealed: &[0; TAG - 1],
            },
        }
        .encode();
        assert_eq!(Msg::decode(&short), Err(MESSAGE));
        // A read with cond 0 but a version, and reserved ops.
        let mut r = Request {
            time: 1,
            op: Op::Read {
                path: "/",
                offset: 0,
                count: 1,
                have: None,
            },
        }
        .encode();
        let n = r.len();
        r[n - 1] = 1;
        assert_eq!(Request::decode(&r), Err((Some(1), Undecoded::Bad)));
        let mut w = 1u64.to_le_bytes().to_vec();
        w.push(0x20);
        assert_eq!(Request::decode(&w), Err((Some(1), Undecoded::Write(0x20))));
        w[8] = 0x40;
        assert_eq!(
            Request::decode(&w),
            Err((Some(1), Undecoded::Unsupported(0x40)))
        );
        assert_eq!(Request::decode(&[1]), Err((None, Undecoded::Bad)));
    }

    #[test]
    fn replies() {
        let d = Dir {
            name: String::from("f"),
            uid: String::from("u"),
            ..Dir::default()
        };
        let packed = d.pack().unwrap();
        for r in [
            Reply::Stat(d.clone()),
            Reply::Read {
                qid: Qid::default(),
                unchanged: false,
                data: b"abc",
            },
            Reply::Read {
                qid: Qid::default(),
                unchanged: true,
                data: b"",
            },
            Reply::Readdir {
                qid: Qid::default(),
                cookie: b"f",
                entries: &packed,
            },
            Reply::Error("permission denied"),
        ] {
            let b = r.encode();
            assert_eq!(Reply::decode(&b).unwrap(), r);
        }
        assert_eq!(
            Reply::Error("permission denied").owned(),
            Err(String::from("permission denied"))
        );
        // Unchanged with data, and entries that do not parse.
        let mut bad = Reply::Read {
            qid: Qid::default(),
            unchanged: true,
            data: b"x",
        }
        .encode();
        assert_eq!(Reply::decode(&bad), Err(MESSAGE));
        bad = Reply::Readdir {
            qid: Qid::default(),
            cookie: b"",
            entries: &[1, 2, 3],
        }
        .encode();
        assert_eq!(Reply::decode(&bad), Err(MESSAGE));
    }
}
