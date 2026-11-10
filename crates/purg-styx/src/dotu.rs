//! A 9P2000 to 9P2000.u translator, for servers that do not speak plain
//! 9P2000. QEMU's virtio-9p server is one: it answers `Tversion "9P2000"`
//! with `"unknown"` and accepts only `"9P2000.u"` and `"9P2000.L"`.
//!
//! [`Shim`] sits between a 9P2000 client and a .u server. It rewrites each
//! T-message on the way down and each R-message on the way up, so neither
//! side sees the other dialect:
//!
//! - `Tversion "9P2000"` goes down as `"9P2000.u"`, and `Rversion "9P2000.u"`
//!   comes back as `"9P2000"`.
//! - `Tauth` and `Tattach` gain `n_uname[4]` ([`NONUNAME`]: no numeric
//!   user). An `Rerror` to `Tauth` comes back as Inferno's
//!   `authentication not required`, since a .u server that does not
//!   authenticate fails the request.
//! - `Rerror` loses `errno[4]`; the error string becomes Inferno's for the
//!   common errnos ([`errstr`]).
//! - `Tcreate` gains an empty `extension[s]`.
//! - Stat entries (`Rstat`, `Twstat`, and directory reads) gain or lose
//!   `extension[s] n_uid[4] n_gid[4] n_muid[4]` ([`stat_from_u`],
//!   [`stat_to_u`]). Directory reads are told apart by the fid: the shim
//!   follows each fid's Qid through attach, walk, open and create. The
//!   `.` and `..` entries QEMU lists are dropped.
//!
//! A translated entry can be longer than the .u one: QEMU sends empty
//! user and group names with numeric ids, which become decimal names. So
//! a directory `Tread` goes down with a smaller count, chosen so that the
//! translated reply never exceeds what the client asked for. A directory
//! read can therefore come back shorter than the client asked even when
//! more entries follow, so the client must read until an empty reply, as
//! 9P requires.
//!
//! Replies that pass through are checked to be well-formed 9P2000 first,
//! so the client is never handed a message it cannot decode.
//!
//! Every other message has the same layout in both dialects and passes
//! through unchanged.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use purg_abi::{auth, error};

use crate::dir::{DMAPPEND, DMAUTH, DMDIR, DMEXCL, Dir, Qid, STATFIXLEN};
use crate::fcall::{Fcall, Msg, VERSION, kind};
use crate::wire::{Reader, Writer};
use crate::{Error, message_size};

/// The version string of the .u dialect.
pub const VERSION_U: &str = "9P2000.u";

/// `n_uname`, `n_uid`, ... for "no numeric id" (and "don't touch" in
/// `Twstat`).
pub const NONUNAME: u32 = !0;

/// Inferno's `DMTMP`: not in [`crate`]'s constants, but a 9P2000 mode bit.
const DMTMP: u32 = 0x0400_0000;
/// The mode bits a 9P2000 stat carries; .u adds symlink, device, pipe,
/// socket, setuid and setgid bits, which are dropped.
const MODE_9P2000: u32 = DMDIR | DMAPPEND | DMEXCL | DMAUTH | DMTMP | 0o777;

/// What a request's reply needs to be translated.
#[derive(Clone, Copy, Debug)]
enum Pending {
    Version,
    Auth,
    Attach { fid: u32 },
    Walk { fid: u32, newfid: u32, n: usize },
    Open { fid: u32 },
    Read { dir: bool },
    Flush { oldtag: u16 },
    Other,
}

/// Translates one 9P2000 conversation to 9P2000.u and back. Feed it every
/// T-message before it goes to the server ([`Shim::request`]) and every
/// R-message before it goes to the client ([`Shim::response`]).
#[derive(Default)]
pub struct Shim {
    /// Outstanding requests by tag.
    tags: BTreeMap<u16, Pending>,
    /// Whether each live fid is a directory.
    dirs: BTreeMap<u32, bool>,
}

impl Shim {
    pub fn new() -> Shim {
        Shim::default()
    }

    /// The .u form of the 9P2000 message `t`.
    pub fn request(&mut self, t: &[u8]) -> Result<Vec<u8>, Error> {
        let msg = Msg::decode(t, u32::MAX)?;
        let tag = msg.tag;
        let (pending, out) = match msg.fcall {
            Fcall::Tversion { msize, version } => {
                // A new version aborts everything outstanding and every fid.
                self.tags.clear();
                self.dirs.clear();
                let version = if version == VERSION {
                    VERSION_U
                } else {
                    version
                };
                let fcall = Fcall::Tversion { msize, version };
                (Pending::Version, Msg { tag, fcall }.encode()?)
            }
            Fcall::Tauth { afid, uname, aname } => {
                let out = build(kind::TAUTH, tag, |w| {
                    w.u32(afid);
                    w.string(uname)?;
                    w.string(aname)?;
                    w.u32(NONUNAME);
                    Ok(())
                })?;
                (Pending::Auth, out)
            }
            Fcall::Tattach {
                fid,
                afid,
                uname,
                aname,
            } => {
                let out = build(kind::TATTACH, tag, |w| {
                    w.u32(fid);
                    w.u32(afid);
                    w.string(uname)?;
                    w.string(aname)?;
                    w.u32(NONUNAME);
                    Ok(())
                })?;
                (Pending::Attach { fid }, out)
            }
            Fcall::Tcreate {
                fid,
                name,
                perm,
                mode,
            } => {
                let out = build(kind::TCREATE, tag, |w| {
                    w.u32(fid);
                    w.string(name)?;
                    w.u32(perm);
                    w.u8(mode);
                    w.string("")
                })?;
                (Pending::Open { fid }, out)
            }
            Fcall::Twstat { fid, stat } => {
                let (d, _) = Dir::unpack(stat)?;
                let stat = stat_to_u(&d)?;
                let out = build(kind::TWSTAT, tag, |w| {
                    w.u32(fid);
                    w.len16(stat.len())?;
                    w.bytes(&stat);
                    Ok(())
                })?;
                (Pending::Other, out)
            }
            Fcall::Twalk {
                fid,
                newfid,
                ref wnames,
            } => (
                Pending::Walk {
                    fid,
                    newfid,
                    n: wnames.len(),
                },
                t.to_vec(),
            ),
            Fcall::Topen { fid, .. } => (Pending::Open { fid }, t.to_vec()),
            Fcall::Tread { fid, offset, count } => {
                let dir = self.dirs.get(&fid).copied().unwrap_or(false);
                let out = if dir {
                    // Ask for less, so that the translated entries still
                    // fit in what the client asked for.
                    let count = dir_count(count);
                    let fcall = Fcall::Tread { fid, offset, count };
                    Msg { tag, fcall }.encode()?
                } else {
                    t.to_vec()
                };
                (Pending::Read { dir }, out)
            }
            Fcall::Tclunk { fid } | Fcall::Tremove { fid } => {
                // The fid is gone whatever the reply says.
                self.dirs.remove(&fid);
                (Pending::Other, t.to_vec())
            }
            Fcall::Tflush { oldtag } => (Pending::Flush { oldtag }, t.to_vec()),
            Fcall::Twrite { .. } | Fcall::Tstat { .. } => (Pending::Other, t.to_vec()),
            other => return Err(Error::BadType(other.kind())),
        };
        self.tags.insert(tag, pending);
        Ok(out)
    }

    /// The 9P2000 form of the .u message `r`.
    pub fn response(&mut self, r: &[u8]) -> Result<Vec<u8>, Error> {
        let size = message_size(r).ok_or(Error::Short)?;
        if size < 7 || size > r.len() {
            return Err(Error::Short);
        }
        if size < r.len() {
            return Err(Error::Trailing);
        }
        let ty = r[4];
        let tag = u16::from_le_bytes([r[5], r[6]]);
        let pending = self.tags.remove(&tag).unwrap_or(Pending::Other);
        let mut body = Reader::new(&r[7..]);
        match (ty, pending) {
            (kind::RERROR, pending) => {
                let ename = body.string()?;
                // The errno is .u's; tolerate a server that leaves it out.
                let errno = body.u32().ok();
                let ename = match pending {
                    Pending::Auth => String::from(auth::NOT_REQUIRED),
                    _ => errstr(errno, ename),
                };
                let fcall = Fcall::Rerror { ename: &ename };
                Msg { tag, fcall }.encode()
            }
            (kind::RVERSION, _) => {
                let Fcall::Rversion { msize, version } = Msg::decode(r, u32::MAX)?.fcall else {
                    return Err(Error::BadType(ty));
                };
                let version = if version == VERSION_U {
                    VERSION
                } else {
                    version
                };
                let fcall = Fcall::Rversion { msize, version };
                Msg { tag, fcall }.encode()
            }
            (kind::RSTAT, _) => {
                // QEMU sends 0 for this `n[2]`; the entry's own size
                // says how long it is.
                let _n = body.u16()?;
                let stat = body.rest();
                let (d, used) = stat_from_u(stat)?;
                if used != stat.len() {
                    return Err(Error::Trailing);
                }
                let stat = d.pack()?;
                Msg {
                    tag,
                    fcall: Fcall::Rstat { stat: &stat },
                }
                .encode()
            }
            (kind::RREAD, Pending::Read { dir: true }) => {
                let n = body.u32()? as usize;
                let mut data = body.bytes(n)?;
                body.finish()?;
                let mut out = Vec::with_capacity(n);
                while !data.is_empty() {
                    let (d, used) = stat_from_u(data)?;
                    // 9P2000 directories do not list themselves or their
                    // parent; QEMU's do.
                    if d.name != "." && d.name != ".." {
                        d.pack_into(&mut out)?;
                    }
                    data = &data[used..];
                }
                Msg {
                    tag,
                    fcall: Fcall::Rread { data: &out },
                }
                .encode()
            }
            (kind::RATTACH, Pending::Attach { fid }) => {
                let out = plain(r)?;
                let qid = Qid::read(&mut body)?;
                self.dirs.insert(fid, qid.is_dir());
                Ok(out)
            }
            (kind::RWALK, Pending::Walk { fid, newfid, n }) => {
                let Fcall::Rwalk { wqids } = Msg::decode(r, u32::MAX)?.fcall else {
                    return Err(Error::BadType(ty));
                };
                // A partial walk leaves newfid unused.
                if wqids.len() == n {
                    let dir = match wqids.last() {
                        Some(q) => q.is_dir(),
                        None => self.dirs.get(&fid).copied().unwrap_or(false),
                    };
                    self.dirs.insert(newfid, dir);
                }
                Ok(r.to_vec())
            }
            (kind::ROPEN | kind::RCREATE, Pending::Open { fid }) => {
                let out = plain(r)?;
                let qid = Qid::read(&mut body)?;
                self.dirs.insert(fid, qid.is_dir());
                Ok(out)
            }
            (kind::RFLUSH, Pending::Flush { oldtag }) => {
                // The flushed request is answered (if at all) before the
                // Rflush.
                self.tags.remove(&oldtag);
                plain(r)
            }
            _ => plain(r),
        }
    }
}

/// An R-message that has the same layout in both dialects, checked to be
/// well-formed 9P2000 so the client never sees a reply it cannot decode.
fn plain(r: &[u8]) -> Result<Vec<u8>, Error> {
    let msg = Msg::decode(r, u32::MAX)?;
    let ty = msg.fcall.kind();
    if ty % 2 == 0 {
        return Err(Error::BadType(ty));
    }
    Ok(r.to_vec())
}

/// The smallest .u stat entry: a 9P2000 entry with empty strings, plus an
/// empty `extension[s]` and three numeric ids.
const USTAT_MIN: u64 = STATFIXLEN as u64 + 2 + 3 * 4;
/// The most a stat entry grows when translated from .u: it loses the .u
/// fields (14 bytes) and three empty names can each become a ten-digit
/// number.
const USTAT_GROWTH: u64 = 3 * 10 - (2 + 3 * 4);

/// The count to ask a .u server for when the client wants `count` bytes of
/// directory: small enough that the translated entries cannot exceed
/// `count`, however many entries come back.
fn dir_count(count: u32) -> u32 {
    (u64::from(count) * USTAT_MIN / (USTAT_MIN + USTAT_GROWTH)) as u32
}

/// Builds a message of type `ty` whose body `body` writes.
fn build(
    ty: u8,
    tag: u16,
    body: impl FnOnce(&mut Writer<'_>) -> Result<(), Error>,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut w = Writer { buf: &mut out };
    w.u32(0);
    w.u8(ty);
    w.u16(tag);
    body(&mut w)?;
    let size = u32::try_from(out.len()).map_err(|_| Error::Overflow)?;
    out[..4].copy_from_slice(&size.to_le_bytes());
    Ok(out)
}

/// Decodes one .u stat entry from the front of `buf`: the 9P2000 [`Dir`]
/// and the bytes used. The .u-only mode bits are dropped. An empty user or
/// group name (QEMU sends only the numbers) becomes the number in decimal.
/// An entry without the .u fields (a plain 9P2000 stat) is accepted too.
pub fn stat_from_u(buf: &[u8]) -> Result<(Dir, usize), Error> {
    let mut r = Reader::new(buf);
    let n = r.u16()? as usize;
    let mut r = Reader::new(r.bytes(n)?);
    let mut d = Dir {
        dtype: r.u16()?,
        dev: r.u32()?,
        qid: Qid::read(&mut r)?,
        mode: r.u32()?,
        atime: r.u32()?,
        mtime: r.u32()?,
        length: r.u64()?,
        name: r.string()?.into(),
        uid: r.string()?.into(),
        gid: r.string()?.into(),
        muid: r.string()?.into(),
    };
    d.mode &= MODE_9P2000;
    if r.finish().is_err() {
        let _extension = r.string()?;
        let n_uid = r.u32()?;
        let n_gid = r.u32()?;
        let n_muid = r.u32()?;
        r.finish()?;
        for (name, id) in [
            (&mut d.uid, n_uid),
            (&mut d.gid, n_gid),
            (&mut d.muid, n_muid),
        ] {
            if name.is_empty() && id != NONUNAME {
                *name = format!("{id}");
            }
        }
    }
    Ok((d, n + 2))
}

/// The .u stat entry for `d`, as `Twstat` sends it: an empty extension and
/// [`NONUNAME`] ids, so only the 9P2000 fields can change anything.
pub fn stat_to_u(d: &Dir) -> Result<Vec<u8>, Error> {
    let mut out = d.pack()?;
    let mut w = Writer { buf: &mut out };
    w.string("")?;
    w.u32(NONUNAME);
    w.u32(NONUNAME);
    w.u32(NONUNAME);
    let body = u16::try_from(out.len() - 2).map_err(|_| Error::Overflow)?;
    out[..2].copy_from_slice(&body.to_le_bytes());
    Ok(out)
}

/// Linux errno values a .u server sends (QEMU sends the host's errno, and
/// translates to Linux values on other hosts).
pub mod errno {
    pub const EPERM: u32 = 1;
    pub const ENOENT: u32 = 2;
    pub const EINTR: u32 = 4;
    pub const EIO: u32 = 5;
    pub const EACCES: u32 = 13;
    pub const EBUSY: u32 = 16;
    pub const EEXIST: u32 = 17;
    pub const ENOTDIR: u32 = 20;
    pub const EISDIR: u32 = 21;
    pub const ENOSPC: u32 = 28;
    pub const ENOTEMPTY: u32 = 39;
}

/// The Inferno error string for a .u `Rerror`: Inferno's string for the
/// errnos it has one for, otherwise `ename` (a host `strerror`) in lower
/// case, as the emu's `#U` reports host errors.
pub fn errstr(errno: Option<u32>, ename: &str) -> String {
    let s = match errno {
        Some(errno::EPERM | errno::EACCES) => error::EPERM,
        Some(errno::ENOENT) => error::ENONEXIST,
        Some(errno::EINTR) => error::EINTR,
        Some(errno::EIO) => error::EIO,
        Some(errno::EBUSY) => error::EINUSE,
        Some(errno::EEXIST) => error::EEXIST,
        Some(errno::ENOTDIR) => error::ENOTDIR,
        Some(errno::EISDIR) => error::EISDIR,
        Some(errno::ENOSPC) => error::ENOSPACE,
        Some(errno::ENOTEMPTY) => error::ENOTEMPTY,
        _ => return ename.to_lowercase(),
    };
    String::from(s)
}
