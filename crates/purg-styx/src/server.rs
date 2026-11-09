//! A small Styx server: turn one `T` message into the `R` message a
//! [`StyxFs`] produces. Version, flush and auth are handled here; the
//! filesystem sees the operations that name a fid.

use alloc::string::String;
use alloc::vec::Vec;

use crate::dir::Qid;
use crate::fcall::{Fcall, Msg, VERSION, kind};

/// Bytes of 9P header in front of a read or write payload (`size` through
/// `count`). A message may carry at most `msize - IOHDRSZ` bytes of data.
pub const IOHDRSZ: u32 = 24;

/// The largest `msize` this framework offers.
pub const MAX_MSIZE: u32 = 8192 + IOHDRSZ;

/// A file tree spoken as 9P2000. Errors are Inferno errstr text and become
/// `Rerror`.
///
/// `walk` returns the qids of the elements that could be walked. An `Err`
/// means the first element failed and no new fid was established. A short
/// `Ok` (fewer qids than names) means the walk stopped; the new fid must not
/// have been established. An empty name list clones `fid` onto `newfid`.
pub trait StyxFs {
    fn set_msize(&mut self, _msize: u32) {}
    fn attach(&mut self, fid: u32, afid: u32, uname: &str, aname: &str) -> Result<Qid, String>;
    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String>;
    fn open(&mut self, fid: u32, mode: u8) -> Result<(Qid, u32), String>;
    fn create(&mut self, fid: u32, name: &str, perm: u32, mode: u8) -> Result<(Qid, u32), String>;
    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String>;
    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String>;
    fn clunk(&mut self, fid: u32) -> Result<(), String>;
    fn remove(&mut self, fid: u32) -> Result<(), String>;
    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String>;
    fn wstat(&mut self, fid: u32, stat: &[u8]) -> Result<(), String>;
}

/// Serves one request. `msize` is updated by `Tversion`. The returned buffer
/// is one whole message, never larger than `msize`.
pub fn dispatch(fs: &mut dyn StyxFs, msg: &Msg<'_>, msize: &mut u32) -> Vec<u8> {
    let tag = msg.tag;
    let answer = match &msg.fcall {
        Fcall::Tversion {
            msize: want,
            version,
        } => {
            let cap = (*msize).clamp(IOHDRSZ, MAX_MSIZE);
            let m = (*want).clamp(IOHDRSZ, cap);
            *msize = m;
            fs.set_msize(m);
            if *version == VERSION {
                Answer::Version {
                    msize: m,
                    version: String::from(VERSION),
                }
            } else {
                Answer::Version {
                    msize: m,
                    version: String::from("unknown"),
                }
            }
        }
        Fcall::Tflush { .. } => Answer::Flush,
        Fcall::Tauth { .. } => Answer::Error(String::from("authentication not required")),
        Fcall::Tattach {
            fid,
            afid,
            uname,
            aname,
        } => match fs.attach(*fid, *afid, uname, aname) {
            Ok(qid) => Answer::Attach(qid),
            Err(e) => Answer::Error(e),
        },
        Fcall::Twalk {
            fid,
            newfid,
            wnames,
        } => match fs.walk(*fid, *newfid, wnames) {
            Ok(q) => Answer::Walk(q),
            Err(e) => Answer::Error(e),
        },
        Fcall::Topen { fid, mode } => match fs.open(*fid, *mode) {
            Ok((qid, iounit)) => Answer::Open { qid, iounit },
            Err(e) => Answer::Error(e),
        },
        Fcall::Tcreate {
            fid,
            name,
            perm,
            mode,
        } => match fs.create(*fid, name, *perm, *mode) {
            Ok((qid, iounit)) => Answer::Create { qid, iounit },
            Err(e) => Answer::Error(e),
        },
        Fcall::Tread { fid, offset, count } => {
            let count = (*count).min(msize.saturating_sub(IOHDRSZ));
            match fs.read(*fid, *offset, count) {
                Ok(mut data) => {
                    let max = msize.saturating_sub(IOHDRSZ) as usize;
                    if data.len() > max {
                        data.truncate(max);
                    }
                    Answer::Read(data)
                }
                Err(e) => Answer::Error(e),
            }
        }
        Fcall::Twrite { fid, offset, data } => match fs.write(*fid, *offset, data) {
            Ok(n) => Answer::Write(n),
            Err(e) => Answer::Error(e),
        },
        Fcall::Tclunk { fid } => match fs.clunk(*fid) {
            Ok(()) => Answer::Clunk,
            Err(e) => Answer::Error(e),
        },
        Fcall::Tremove { fid } => match fs.remove(*fid) {
            Ok(()) => Answer::Remove,
            Err(e) => Answer::Error(e),
        },
        Fcall::Tstat { fid } => match fs.stat(*fid) {
            Ok(s) => Answer::Stat(s),
            Err(e) => Answer::Error(e),
        },
        Fcall::Twstat { fid, stat } => match fs.wstat(*fid, stat) {
            Ok(()) => Answer::Wstat,
            Err(e) => Answer::Error(e),
        },
        _ => Answer::Error(String::from("bad 9P message type")),
    };
    encode(tag, &answer, *msize)
}

enum Answer {
    Version { msize: u32, version: String },
    Error(String),
    Attach(Qid),
    Flush,
    Walk(Vec<Qid>),
    Open { qid: Qid, iounit: u32 },
    Create { qid: Qid, iounit: u32 },
    Read(Vec<u8>),
    Write(u32),
    Clunk,
    Remove,
    Stat(Vec<u8>),
    Wstat,
}

fn encode(tag: u16, a: &Answer, msize: u32) -> Vec<u8> {
    let fallback = || {
        let f = Fcall::Rerror { ename: "i/o error" };
        Msg { tag, fcall: f }.encode().unwrap_or_else(|_| {
            // A tiny legal Rerror, size included.
            let mut v = Vec::new();
            v.extend_from_slice(&19u32.to_le_bytes());
            v.push(kind::RERROR);
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&7u16.to_le_bytes());
            v.extend_from_slice(b"i/o error");
            v
        })
    };
    let msg = match a {
        Answer::Version { msize, version } => Msg {
            tag,
            fcall: Fcall::Rversion {
                msize: *msize,
                version,
            },
        },
        Answer::Error(e) => Msg {
            tag,
            fcall: Fcall::Rerror { ename: e },
        },
        Answer::Attach(qid) => Msg {
            tag,
            fcall: Fcall::Rattach { qid: *qid },
        },
        Answer::Flush => Msg {
            tag,
            fcall: Fcall::Rflush,
        },
        Answer::Walk(q) => Msg {
            tag,
            fcall: Fcall::Rwalk { wqids: q.clone() },
        },
        Answer::Open { qid, iounit } => Msg {
            tag,
            fcall: Fcall::Ropen {
                qid: *qid,
                iounit: *iounit,
            },
        },
        Answer::Create { qid, iounit } => Msg {
            tag,
            fcall: Fcall::Rcreate {
                qid: *qid,
                iounit: *iounit,
            },
        },
        Answer::Read(data) => Msg {
            tag,
            fcall: Fcall::Rread { data },
        },
        Answer::Write(n) => Msg {
            tag,
            fcall: Fcall::Rwrite { count: *n },
        },
        Answer::Clunk => Msg {
            tag,
            fcall: Fcall::Rclunk,
        },
        Answer::Remove => Msg {
            tag,
            fcall: Fcall::Rremove,
        },
        Answer::Stat(stat) => Msg {
            tag,
            fcall: Fcall::Rstat { stat },
        },
        Answer::Wstat => Msg {
            tag,
            fcall: Fcall::Rwstat,
        },
    };
    match msg.encode() {
        Ok(v) if v.len() <= msize as usize => v,
        Ok(_) => fallback(),
        Err(_) => fallback(),
    }
}
