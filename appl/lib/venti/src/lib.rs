//! Venti: the client side of the Venti protocol (Inferno's venti(2),
//! Plan 9's venti(6)) and, in [`vac`], the vac archive format that
//! `vacput`, `vacget` and `vacfs` share.
//!
//! A Venti server stores blocks by the SHA-1 hash of their contents, the
//! block's *score*. Each block also carries a type. The types here are
//! the ones on the wire and on disk: Plan 9's libventi translates its own
//! numbering to these (`vttodisktype`), and Inferno uses them directly.
//!
//! Every integer on the wire and in the archive structures is big-endian.
//! A message is `size[2]` (the length of what follows), `type[1]`,
//! `tag[1]` and the body. A connection starts with each side writing a
//! line `venti-<versions>-<comment>`, then a hello transaction.

pub mod sha1;
pub mod vac;

use purgatorio::fs::File;
use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};

pub const SCORESIZE: usize = 20;
pub const MAXSTRINGSIZE: usize = 1000;
/// The largest block a server accepts.
pub const MAXLUMPSIZE: usize = 56 * 1024;
/// The deepest tree of pointer blocks an entry can describe.
pub const POINTERDEPTH: u8 = 7;
pub const ENTRYSIZE: usize = 40;
pub const ROOTSIZE: usize = 300;
pub const ROOTVERSION: u16 = 2;
/// Plan 9's `_VtRootVersionBig`: the root's block size is in the
/// `(n>>5)<<(n&31)` form.
const ROOTVERSIONBIG: u16 = 1 << 15;
const ROOTNAMELEN: usize = 128;
pub const MAXFILESIZE: u64 = (1 << 48) - 1;

// Block types, as on the wire.
pub const ERRTYPE: u8 = 0;
pub const ROOTTYPE: u8 = 1;
pub const DIRTYPE: u8 = 2;
/// A pointer block one level above the data; `POINTERTYPE0 + n` is
/// `n` levels higher.
pub const POINTERTYPE0: u8 = 3;
pub const DATATYPE: u8 = 13;
pub const MAXTYPE: u8 = 14;

// Entry flags.
pub const ENTRYACTIVE: u8 = 1 << 0;
pub const ENTRYDIR: u8 = 1 << 1;
pub const ENTRYDEPTHSHIFT: u8 = 2;
pub const ENTRYDEPTHMASK: u8 = 7 << 2;
pub const ENTRYLOCAL: u8 = 1 << 5;
/// Plan 9's `_VtEntryBig`: psize and dsize are in the `(n>>5)<<(n&31)`
/// form.
const ENTRYBIG: u8 = 1 << 6;

// Message types.
pub const RERROR: u8 = 1;
pub const TPING: u8 = 2;
pub const RPING: u8 = 3;
pub const THELLO: u8 = 4;
pub const RHELLO: u8 = 5;
pub const TGOODBYE: u8 = 6;
pub const TREAD: u8 = 12;
pub const RREAD: u8 = 13;
pub const TWRITE: u8 = 14;
pub const RWRITE: u8 = 15;
pub const TSYNC: u8 = 16;
pub const RSYNC: u8 = 17;

/// The protocol versions this side speaks, in order of preference.
pub const VERSIONS: &[&str] = &["02"];

/// Where the commands dial when not given `-a`: the `venti` attribute
/// of the local system in the network database.
pub const DEFAULT_ADDR: &str = "$venti";

/// The score of the empty block, SHA-1 of nothing. A server never needs
/// to be asked for it.
const ZERO: [u8; SCORESIZE] = [
    0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90,
    0xaf, 0xd8, 0x07, 0x09,
];

/// The name of a block: the SHA-1 hash of its contents.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Score(pub [u8; SCORESIZE]);

impl Score {
    /// The score of the empty block.
    pub fn zero() -> Score {
        Score(ZERO)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == ZERO
    }

    /// The score of a block holding `data`.
    pub fn of(data: &[u8]) -> Score {
        Score(sha1::sha1(data))
    }

    /// Parses 40 hexadecimal digits.
    pub fn parse(s: &str) -> Option<Score> {
        let b = s.as_bytes();
        if b.len() != 2 * SCORESIZE {
            return None;
        }
        let mut out = [0u8; SCORESIZE];
        for (i, c) in b.iter().enumerate() {
            let v = (*c as char).to_digit(16)? as u8;
            out[i / 2] |= if i % 2 == 0 { v << 4 } else { v };
        }
        Some(Score(out))
    }

    fn from_slice(b: &[u8]) -> Score {
        let mut s = [0u8; SCORESIZE];
        s.copy_from_slice(&b[..SCORESIZE]);
        Score(s)
    }
}

impl fmt::Display for Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// `data` without what a server would drop before storing it as a block
/// of `btype`: trailing zero bytes of a data or directory block, trailing
/// zero scores (and a partial score) of a pointer block. A block's score
/// is the hash of what is left, so writers trim before writing.
pub fn zero_truncate(btype: u8, data: &[u8]) -> &[u8] {
    match btype {
        ROOTTYPE => &data[..data.len().min(ROOTSIZE)],
        DATATYPE | DIRTYPE => {
            let n = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            &data[..n]
        }
        _ => {
            let mut n = data.len() / SCORESIZE * SCORESIZE;
            while n >= SCORESIZE && data[n - SCORESIZE..n] == ZERO {
                n -= SCORESIZE;
            }
            &data[..n]
        }
    }
}

fn get16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn get32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn get48(b: &[u8], o: usize) -> u64 {
    (u64::from(get16(b, o)) << 32) | u64::from(get32(b, o + 2))
}

/// The `(n>>5)<<(n&31)` form Plan 9 uses for sizes of 64 KiB and more.
fn unbig(n: u16) -> u32 {
    u32::from(n >> 5) << (n & 31)
}

/// A NUL-padded string in a fixed field.
fn get_fixed(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// The root block of an archive: what a `vac:` score names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub name: String,
    /// `vac` for a vac archive.
    pub rtype: String,
    /// The block of the three top-level entries.
    pub score: Score,
    pub blocksize: u32,
    /// The archive's previous root, if any.
    pub prev: Option<Score>,
}

impl Root {
    /// `version[2] name[128] type[128] score[20] blocksize[2] prev[20]`.
    pub fn pack(&self) -> Result<[u8; ROOTSIZE], String> {
        let mut d = [0u8; ROOTSIZE];
        d[0..2].copy_from_slice(&ROOTVERSION.to_be_bytes());
        for (o, s) in [(2, &self.name), (2 + ROOTNAMELEN, &self.rtype)] {
            let b = s.as_bytes();
            if b.len() > ROOTNAMELEN {
                return Err(format!("string too long: {s}"));
            }
            d[o..o + b.len()].copy_from_slice(b);
        }
        let o = 2 + 2 * ROOTNAMELEN;
        d[o..o + SCORESIZE].copy_from_slice(&self.score.0);
        let bs = u16::try_from(self.blocksize).map_err(|_| String::from("bad block size"))?;
        d[o + SCORESIZE..o + SCORESIZE + 2].copy_from_slice(&bs.to_be_bytes());
        if let Some(p) = self.prev {
            d[o + SCORESIZE + 2..].copy_from_slice(&p.0);
        }
        Ok(d)
    }

    pub fn unpack(d: &[u8]) -> Result<Root, String> {
        if d.len() != ROOTSIZE {
            return Err(String::from("root entry is wrong length"));
        }
        let version = get16(d, 0);
        if version & !ROOTVERSIONBIG != ROOTVERSION {
            return Err(String::from("unknown root version"));
        }
        let o = 2 + 2 * ROOTNAMELEN;
        let bs = get16(d, o + SCORESIZE);
        let blocksize = if version & ROOTVERSIONBIG != 0 {
            unbig(bs)
        } else {
            u32::from(bs)
        };
        if blocksize < 256 {
            return Err(String::from("bad block size"));
        }
        let prev = Score::from_slice(&d[o + SCORESIZE + 2..]);
        Ok(Root {
            name: get_fixed(&d[2..2 + ROOTNAMELEN]),
            rtype: get_fixed(&d[2 + ROOTNAMELEN..o]),
            score: Score::from_slice(&d[o..]),
            blocksize,
            prev: (prev.0 != [0; SCORESIZE]).then_some(prev),
        })
    }
}

/// A stream of blocks: a tree of pointer blocks `depth` deep over data
/// blocks of `dsize` bytes, `size` bytes in all. A directory stream
/// (`ENTRYDIR`) holds entries of further streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub generation: u32,
    /// Bytes in a pointer block.
    pub psize: u32,
    /// Bytes in a data block.
    pub dsize: u32,
    pub depth: u8,
    /// The flags without the depth.
    pub flags: u8,
    pub size: u64,
    pub score: Score,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & ENTRYDIR != 0
    }

    pub fn is_active(&self) -> bool {
        self.flags & ENTRYACTIVE != 0
    }

    /// `gen[4] psize[2] dsize[2] flags[1] zero[5] size[6] score[20]`.
    pub fn pack(&self) -> [u8; ENTRYSIZE] {
        let mut d = [0u8; ENTRYSIZE];
        d[0..4].copy_from_slice(&self.generation.to_be_bytes());
        d[4..6].copy_from_slice(&(self.psize as u16).to_be_bytes());
        d[6..8].copy_from_slice(&(self.dsize as u16).to_be_bytes());
        d[8] = (self.flags & !(ENTRYDEPTHMASK | ENTRYBIG)) | (self.depth << ENTRYDEPTHSHIFT);
        d[14..20].copy_from_slice(&self.size.to_be_bytes()[2..]);
        d[20..40].copy_from_slice(&self.score.0);
        d
    }

    pub fn unpack(d: &[u8]) -> Result<Entry, String> {
        if d.len() != ENTRYSIZE {
            return Err(String::from("entry is wrong length"));
        }
        let raw = d[8];
        let (mut psize, mut dsize) = (u32::from(get16(d, 4)), u32::from(get16(d, 6)));
        if raw & ENTRYBIG != 0 {
            psize = unbig(psize as u16);
            dsize = unbig(dsize as u16);
        }
        let e = Entry {
            generation: get32(d, 0),
            psize,
            dsize,
            depth: (raw & ENTRYDEPTHMASK) >> ENTRYDEPTHSHIFT,
            flags: raw & !(ENTRYDEPTHMASK | ENTRYBIG),
            size: get48(d, 14),
            score: Score::from_slice(&d[20..]),
        };
        if e.is_active() && (e.psize < 256 || e.dsize < 256) {
            // Old archives give an empty file sizes of 0.
            if e.psize == 0 && e.dsize == 0 && e.score.is_zero() {
                return Ok(Entry {
                    psize: 4096,
                    dsize: 4096,
                    ..e
                });
            }
            return Err(format!("bad blocksize ({} or {})", e.psize, e.dsize));
        }
        Ok(e)
    }
}

/// A Venti message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Rerror(String),
    Tping,
    Rping,
    Thello {
        version: String,
        uid: String,
        strength: u8,
        crypto: Vec<u8>,
        codec: Vec<u8>,
    },
    Rhello {
        sid: String,
        crypto: u8,
        codec: u8,
    },
    Tgoodbye,
    Tread {
        score: Score,
        btype: u8,
        count: u32,
    },
    Rread(Vec<u8>),
    Twrite {
        btype: u8,
        data: Vec<u8>,
    },
    Rwrite(Score),
    Tsync,
    Rsync,
}

fn put_string(d: &mut Vec<u8>, s: &str) -> Result<(), String> {
    if s.len() > MAXSTRINGSIZE {
        return Err(String::from("string too long in packet"));
    }
    d.extend_from_slice(&(s.len() as u16).to_be_bytes());
    d.extend_from_slice(s.as_bytes());
    Ok(())
}

/// A cursor over a message body.
struct Get<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> Get<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.o + n > self.b.len() {
            return Err(String::from("bad venti message format"));
        }
        let s = &self.b[self.o..self.o + n];
        self.o += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn string(&mut self) -> Result<String, String> {
        let n = get16(self.bytes(2)?, 0) as usize;
        if n > MAXSTRINGSIZE {
            return Err(String::from("string too long in packet"));
        }
        Ok(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }

    fn counted(&mut self) -> Result<Vec<u8>, String> {
        let n = self.u8()? as usize;
        Ok(self.bytes(n)?.to_vec())
    }

    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.o..];
        self.o = self.b.len();
        s
    }
}

impl Msg {
    pub fn mtype(&self) -> u8 {
        match self {
            Msg::Rerror(_) => RERROR,
            Msg::Tping => TPING,
            Msg::Rping => RPING,
            Msg::Thello { .. } => THELLO,
            Msg::Rhello { .. } => RHELLO,
            Msg::Tgoodbye => TGOODBYE,
            Msg::Tread { .. } => TREAD,
            Msg::Rread(_) => RREAD,
            Msg::Twrite { .. } => TWRITE,
            Msg::Rwrite(_) => RWRITE,
            Msg::Tsync => TSYNC,
            Msg::Rsync => RSYNC,
        }
    }

    /// The message with `tag`, framed with its `size[2]`.
    pub fn pack(&self, tag: u8) -> Result<Vec<u8>, String> {
        let mut d = vec![0, 0, self.mtype(), tag];
        match self {
            Msg::Rerror(e) => put_string(&mut d, e)?,
            Msg::Thello {
                version,
                uid,
                strength,
                crypto,
                codec,
            } => {
                put_string(&mut d, version)?;
                put_string(&mut d, uid)?;
                d.push(*strength);
                for l in [crypto, codec] {
                    d.push(u8::try_from(l.len()).map_err(|_| "list too long")?);
                    d.extend_from_slice(l);
                }
            }
            Msg::Rhello { sid, crypto, codec } => {
                put_string(&mut d, sid)?;
                d.push(*crypto);
                d.push(*codec);
            }
            Msg::Tread {
                score,
                btype,
                count,
            } => {
                d.extend_from_slice(&score.0);
                d.push(*btype);
                d.push(0);
                let c = u16::try_from(*count)
                    .map_err(|_| String::from("read count too large for protocol"))?;
                d.extend_from_slice(&c.to_be_bytes());
            }
            Msg::Rread(data) => d.extend_from_slice(data),
            Msg::Twrite { btype, data } => {
                d.extend_from_slice(&[*btype, 0, 0, 0]);
                d.extend_from_slice(data);
            }
            Msg::Rwrite(score) => d.extend_from_slice(&score.0),
            Msg::Tping | Msg::Rping | Msg::Tgoodbye | Msg::Tsync | Msg::Rsync => {}
        }
        let n = u16::try_from(d.len() - 2).map_err(|_| String::from("packet too large"))?;
        d[0..2].copy_from_slice(&n.to_be_bytes());
        Ok(d)
    }

    /// Decodes a message without its `size[2]`: `(tag, message)`.
    pub fn unpack(body: &[u8]) -> Result<(u8, Msg), String> {
        if body.len() < 2 {
            return Err(String::from("message too small"));
        }
        let (mtype, tag) = (body[0], body[1]);
        let mut g = Get { b: body, o: 2 };
        let m = match mtype {
            RERROR => Msg::Rerror(g.string()?),
            TPING => Msg::Tping,
            RPING => Msg::Rping,
            THELLO => Msg::Thello {
                version: g.string()?,
                uid: g.string()?,
                strength: g.u8()?,
                crypto: g.counted()?,
                codec: g.counted()?,
            },
            RHELLO => Msg::Rhello {
                sid: g.string()?,
                crypto: g.u8()?,
                codec: g.u8()?,
            },
            TGOODBYE => Msg::Tgoodbye,
            TREAD => {
                let score = Score::from_slice(g.bytes(SCORESIZE)?);
                let btype = g.u8()?;
                g.u8()?;
                // Version 04 of the protocol has a four-byte count.
                let c = g.rest();
                let count = match c.len() {
                    2 => u32::from(get16(c, 0)),
                    4 => get32(c, 0),
                    _ => return Err(String::from("bad venti message format")),
                };
                Msg::Tread {
                    score,
                    btype,
                    count,
                }
            }
            RREAD => Msg::Rread(g.rest().to_vec()),
            TWRITE => {
                let btype = g.u8()?;
                g.bytes(3)?;
                Msg::Twrite {
                    btype,
                    data: g.rest().to_vec(),
                }
            }
            RWRITE => Msg::Rwrite(Score::from_slice(g.bytes(SCORESIZE)?)),
            TSYNC => Msg::Tsync,
            RSYNC => Msg::Rsync,
            _ => return Err(format!("unrecognised mtype {mtype}")),
        };
        if g.o != body.len() {
            return Err(String::from("bad message size"));
        }
        Ok((tag, m))
    }
}

/// Reads `buf.len()` bytes; `Ok(false)` at end of file before the first.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<bool, String> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(String::from("venti message truncated")),
            Ok(n) => got += n,
            Err(e) => return Err(purgatorio::Error::from(e).0),
        }
    }
    Ok(true)
}

/// Reads one message body (without its `size[2]`); `Ok(None)` at a clean
/// end of file.
pub fn read_frame(r: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut hdr = [0u8; 2];
    if !read_full(r, &mut hdr)? {
        return Ok(None);
    }
    let n = u16::from_be_bytes(hdr) as usize;
    if n < 2 {
        return Err(String::from("invalid venti message size"));
    }
    let mut body = vec![0u8; n];
    if !read_full(r, &mut body)? {
        return Err(String::from("venti message truncated"));
    }
    Ok(Some(body))
}

/// The opening exchange, the same from either side: writes this side's
/// version line, reads the peer's, and returns the first version in the
/// peer's list that this side speaks.
pub fn version(io: &mut (impl Read + Write)) -> Result<String, String> {
    let line = format!("venti-{}-libventi\n", VERSIONS.join(":"));
    io.write_all(line.as_bytes())
        .map_err(|e| purgatorio::Error::from(e).0)?;
    let prefix = b"venti-";
    let mut v = Vec::new();
    loop {
        if v.len() >= MAXSTRINGSIZE {
            return Err(String::from("initial version string too long"));
        }
        let mut c = [0u8; 1];
        if !read_full(io, &mut c)? {
            return Err(String::from("eof on version string"));
        }
        let c = c[0];
        if c == b'\n' {
            break;
        }
        if !(b' '..=0x7f).contains(&c) || (v.len() < prefix.len() && prefix[v.len()] != c) {
            return Err(String::from("bad version string"));
        }
        v.push(c);
    }
    if v.len() < prefix.len() {
        return Err(String::from("bad version string"));
    }
    let v = String::from_utf8_lossy(&v[prefix.len()..]).into_owned();
    // `versions-comment`: the versions are colon-separated.
    let list = v.split('-').next().unwrap_or("");
    list.split(':')
        .find(|x| VERSIONS.contains(x))
        .map(String::from)
        .ok_or_else(|| String::from("unknown version"))
}

/// Block storage by score: a [`Session`] with a server, or a [`MemStore`].
pub trait Venti {
    /// The block `score` of type `btype`, at most `max` bytes.
    fn read(&mut self, score: &Score, btype: u8, max: usize) -> Result<Vec<u8>, String>;
    /// Stores `data` as a block of `btype` and returns its score.
    fn write(&mut self, btype: u8, data: &[u8]) -> Result<Score, String>;
}

/// A session with a Venti server over `io`.
pub struct Session<S: Read + Write> {
    io: S,
    /// The protocol version agreed.
    pub version: String,
    /// The server's name for itself, from its hello.
    pub sid: String,
}

impl<S: Read + Write> Session<S> {
    /// Agrees a version and says hello.
    pub fn new(mut io: S) -> Result<Session<S>, String> {
        let version = version(&mut io)?;
        let mut s = Session {
            io,
            version: version.clone(),
            sid: String::new(),
        };
        let hello = Msg::Thello {
            version,
            uid: String::new(),
            strength: 0,
            crypto: Vec::new(),
            codec: Vec::new(),
        };
        match s.rpc(hello) {
            Ok(Msg::Rhello { sid, .. }) => s.sid = sid,
            Ok(_) => return Err(String::from("hello failed: reply message is of wrong type")),
            Err(e) => return Err(format!("hello failed: {e}")),
        }
        Ok(s)
    }

    /// Sends `m` and returns the reply; an `Rerror` is its error string.
    pub fn rpc(&mut self, m: Msg) -> Result<Msg, String> {
        let tag = 0;
        let d = m.pack(tag)?;
        self.io
            .write_all(&d)
            .map_err(|_| String::from("write failed"))?;
        let body = read_frame(&mut self.io)?.ok_or_else(|| String::from("eof reading message"))?;
        let (rtag, r) = Msg::unpack(&body)?;
        if rtag != tag {
            return Err(String::from("message tags don't match"));
        }
        if let Msg::Rerror(e) = r {
            return Err(e);
        }
        if r.mtype() & 1 == 0 {
            return Err(String::from("reply message is a t-message"));
        }
        if r.mtype() != m.mtype() + 1 {
            return Err(String::from("reply message is of wrong type"));
        }
        Ok(r)
    }

    pub fn ping(&mut self) -> Result<(), String> {
        self.rpc(Msg::Tping).map(drop)
    }

    /// Asks the server to commit what it has been given to stable
    /// storage.
    pub fn sync(&mut self) -> Result<(), String> {
        self.rpc(Msg::Tsync).map(drop)
    }

    /// Ends the session. The server sends no reply.
    pub fn goodbye(mut self) -> Result<(), String> {
        let d = Msg::Tgoodbye.pack(0)?;
        self.io
            .write_all(&d)
            .map_err(|_| String::from("write failed"))
    }
}

impl<S: Read + Write> Venti for Session<S> {
    fn read(&mut self, score: &Score, btype: u8, max: usize) -> Result<Vec<u8>, String> {
        if score.is_zero() {
            return Ok(Vec::new());
        }
        let r = self.rpc(Msg::Tread {
            score: *score,
            btype,
            count: max as u32,
        })?;
        let Msg::Rread(data) = r else {
            return Err(String::from("reply message is of wrong type"));
        };
        if data.len() > max {
            return Err(String::from("read returned too much data"));
        }
        let got = Score::of(&data);
        if got != *score {
            return Err(format!("read asked for {score} got {got}"));
        }
        Ok(data)
    }

    fn write(&mut self, btype: u8, data: &[u8]) -> Result<Score, String> {
        if data.is_empty() {
            return Ok(Score::zero());
        }
        let want = Score::of(data);
        let r = self.rpc(Msg::Twrite {
            btype,
            data: data.to_vec(),
        })?;
        let Msg::Rwrite(score) = r else {
            return Err(String::from("reply message is of wrong type"));
        };
        if score != want {
            return Err(format!("sha1 hash mismatch: want {want} got {score}"));
        }
        Ok(score)
    }
}

/// Blocks held in memory: the store of the test server, and of tests.
#[derive(Default)]
pub struct MemStore {
    blocks: HashMap<Score, (u8, Vec<u8>)>,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    /// The number of blocks held.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl Venti for MemStore {
    fn read(&mut self, score: &Score, btype: u8, max: usize) -> Result<Vec<u8>, String> {
        if score.is_zero() {
            return Ok(Vec::new());
        }
        match self.blocks.get(score) {
            Some((t, d)) if *t == btype => {
                if d.len() > max {
                    return Err(format!(
                        "block {score} is {} bytes, more than {max}",
                        d.len()
                    ));
                }
                Ok(d.clone())
            }
            _ => Err(format!("no block with score {score}/{btype} exists")),
        }
    }

    fn write(&mut self, btype: u8, data: &[u8]) -> Result<Score, String> {
        if data.len() > MAXLUMPSIZE {
            return Err(String::from("block too big"));
        }
        if btype == ERRTYPE || btype >= MAXTYPE {
            return Err(format!("bad block type {btype}"));
        }
        let s = Score::of(data);
        self.blocks.insert(s, (btype, data.to_vec()));
        Ok(s)
    }
}

/// Fills in the network and service of an address as Inferno's
/// `dial->netmkaddr` does: `host` becomes `net!host!svc`, `net!host`
/// becomes `net!host!svc`, and an address with a service is left alone.
pub fn netmkaddr(addr: &str, net: &str, svc: &str) -> String {
    match addr.split('!').filter(|s| !s.is_empty()).count() {
        0 | 1 => format!("{net}!{addr}!{svc}"),
        2 => format!("{addr}!{svc}"),
        _ => String::from(addr),
    }
}

/// Dials the Venti server at `addr` (`$venti` by default, the `venti`
/// service of TCP) and starts a session. The errors read as Inferno's
/// commands print them.
pub fn dial(addr: &str) -> Result<Session<File>, String> {
    let addr = netmkaddr(addr, "net", "venti");
    let mut conn = purgatorio::dial::dial(&addr).map_err(|e| format!("dialing {addr}: {}", e.0))?;
    let fd = conn
        .data
        .take()
        .ok_or_else(|| format!("dialing {addr}: no data file"))?;
    Session::new(File::from(fd)).map_err(|e| format!("handshake: {e}"))
}

/// Parses a `[tag:]score` argument, the tag `vac` if absent.
pub fn parse_tagged(s: &str, want: &str) -> Result<Score, String> {
    let (tag, hex) = match s.rfind(':') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (want, s),
    };
    if !tag.is_empty() && tag != want {
        return Err(format!("bad score type: {tag}"));
    }
    Score::parse(hex).ok_or_else(|| format!("bad score: {hex}"))
}

#[cfg(test)]
mod tests;
