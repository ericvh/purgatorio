use alloc::vec::Vec;

use crate::Error;
use crate::dir::Qid;
use crate::wire::{Reader, Writer};

/// The protocol version string. Inferno's Styx is 9P2000.
pub const VERSION: &str = "9P2000";
pub const NOTAG: u16 = 0xffff;
pub const NOFID: u32 = 0xffff_ffff;
/// Maximum number of elements in one walk.
pub const MAXWELEM: usize = 16;

/// Message type bytes.
pub mod kind {
    pub const TVERSION: u8 = 100;
    pub const RVERSION: u8 = 101;
    pub const TAUTH: u8 = 102;
    pub const RAUTH: u8 = 103;
    pub const TATTACH: u8 = 104;
    pub const RATTACH: u8 = 105;
    pub const TERROR: u8 = 106;
    pub const RERROR: u8 = 107;
    pub const TFLUSH: u8 = 108;
    pub const RFLUSH: u8 = 109;
    pub const TWALK: u8 = 110;
    pub const RWALK: u8 = 111;
    pub const TOPEN: u8 = 112;
    pub const ROPEN: u8 = 113;
    pub const TCREATE: u8 = 114;
    pub const RCREATE: u8 = 115;
    pub const TREAD: u8 = 116;
    pub const RREAD: u8 = 117;
    pub const TWRITE: u8 = 118;
    pub const RWRITE: u8 = 119;
    pub const TCLUNK: u8 = 120;
    pub const RCLUNK: u8 = 121;
    pub const TREMOVE: u8 = 122;
    pub const RREMOVE: u8 = 123;
    pub const TSTAT: u8 = 124;
    pub const RSTAT: u8 = 125;
    pub const TWSTAT: u8 = 126;
    pub const RWSTAT: u8 = 127;
}

/// A 9P2000 message body. Borrowed fields point into the decoded buffer.
///
/// `stat` fields hold one packed [`crate::Dir`] (starting with its own
/// `size[2]`); the codec adds and strips the extra `n[2]` prefix that 9P
/// puts in front of stat data in `Rstat` and `Twstat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fcall<'a> {
    Tversion {
        msize: u32,
        version: &'a str,
    },
    Rversion {
        msize: u32,
        version: &'a str,
    },
    Tauth {
        afid: u32,
        uname: &'a str,
        aname: &'a str,
    },
    Rauth {
        aqid: Qid,
    },
    Tattach {
        fid: u32,
        afid: u32,
        uname: &'a str,
        aname: &'a str,
    },
    Rattach {
        qid: Qid,
    },
    Rerror {
        ename: &'a str,
    },
    Tflush {
        oldtag: u16,
    },
    Rflush,
    Twalk {
        fid: u32,
        newfid: u32,
        wnames: Vec<&'a str>,
    },
    Rwalk {
        wqids: Vec<Qid>,
    },
    Topen {
        fid: u32,
        mode: u8,
    },
    Ropen {
        qid: Qid,
        iounit: u32,
    },
    Tcreate {
        fid: u32,
        name: &'a str,
        perm: u32,
        mode: u8,
    },
    Rcreate {
        qid: Qid,
        iounit: u32,
    },
    Tread {
        fid: u32,
        offset: u64,
        count: u32,
    },
    Rread {
        data: &'a [u8],
    },
    Twrite {
        fid: u32,
        offset: u64,
        data: &'a [u8],
    },
    Rwrite {
        count: u32,
    },
    Tclunk {
        fid: u32,
    },
    Rclunk,
    Tremove {
        fid: u32,
    },
    Rremove,
    Tstat {
        fid: u32,
    },
    Rstat {
        stat: &'a [u8],
    },
    Twstat {
        fid: u32,
        stat: &'a [u8],
    },
    Rwstat,
}

impl Fcall<'_> {
    pub fn kind(&self) -> u8 {
        use kind::*;
        match self {
            Fcall::Tversion { .. } => TVERSION,
            Fcall::Rversion { .. } => RVERSION,
            Fcall::Tauth { .. } => TAUTH,
            Fcall::Rauth { .. } => RAUTH,
            Fcall::Tattach { .. } => TATTACH,
            Fcall::Rattach { .. } => RATTACH,
            Fcall::Rerror { .. } => RERROR,
            Fcall::Tflush { .. } => TFLUSH,
            Fcall::Rflush => RFLUSH,
            Fcall::Twalk { .. } => TWALK,
            Fcall::Rwalk { .. } => RWALK,
            Fcall::Topen { .. } => TOPEN,
            Fcall::Ropen { .. } => ROPEN,
            Fcall::Tcreate { .. } => TCREATE,
            Fcall::Rcreate { .. } => RCREATE,
            Fcall::Tread { .. } => TREAD,
            Fcall::Rread { .. } => RREAD,
            Fcall::Twrite { .. } => TWRITE,
            Fcall::Rwrite { .. } => RWRITE,
            Fcall::Tclunk { .. } => TCLUNK,
            Fcall::Rclunk => RCLUNK,
            Fcall::Tremove { .. } => TREMOVE,
            Fcall::Rremove => RREMOVE,
            Fcall::Tstat { .. } => TSTAT,
            Fcall::Rstat { .. } => RSTAT,
            Fcall::Twstat { .. } => TWSTAT,
            Fcall::Rwstat => RWSTAT,
        }
    }
}

/// A complete message: tag plus body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg<'a> {
    pub tag: u16,
    pub fcall: Fcall<'a>,
}

impl<'a> Msg<'a> {
    /// Decodes exactly one message occupying all of `buf`. Messages whose
    /// `size[4]` exceeds `msize` are rejected.
    pub fn decode(buf: &'a [u8], msize: u32) -> Result<Msg<'a>, Error> {
        let mut r = Reader::new(buf);
        let size = r.u32()? as usize;
        if size > msize as usize {
            return Err(Error::TooBig);
        }
        if size < 7 {
            return Err(Error::Short);
        }
        if size > buf.len() {
            return Err(Error::Short);
        }
        if size < buf.len() {
            return Err(Error::Trailing);
        }
        let ty = r.u8()?;
        let tag = r.u16()?;
        let fcall = decode_body(ty, &mut r)?;
        r.finish()?;
        Ok(Msg { tag, fcall })
    }

    /// Appends the encoded message to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let start = out.len();
        let mut w = Writer { buf: out };
        w.u32(0);
        w.u8(self.fcall.kind());
        w.u16(self.tag);
        encode_body(&self.fcall, &mut w)?;
        let size = u32::try_from(out.len() - start).map_err(|_| Error::Overflow)?;
        out[start..start + 4].copy_from_slice(&size.to_le_bytes());
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut v = Vec::new();
        self.encode_into(&mut v)?;
        Ok(v)
    }
}

fn data<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], Error> {
    let n = r.u32()? as usize;
    r.bytes(n)
}

fn stat<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], Error> {
    let n = r.u16()? as usize;
    r.bytes(n)
}

fn decode_body<'a>(ty: u8, r: &mut Reader<'a>) -> Result<Fcall<'a>, Error> {
    use kind::*;
    Ok(match ty {
        TVERSION => Fcall::Tversion {
            msize: r.u32()?,
            version: r.string()?,
        },
        RVERSION => Fcall::Rversion {
            msize: r.u32()?,
            version: r.string()?,
        },
        TAUTH => Fcall::Tauth {
            afid: r.u32()?,
            uname: r.string()?,
            aname: r.string()?,
        },
        RAUTH => Fcall::Rauth {
            aqid: Qid::read(r)?,
        },
        TATTACH => Fcall::Tattach {
            fid: r.u32()?,
            afid: r.u32()?,
            uname: r.string()?,
            aname: r.string()?,
        },
        RATTACH => Fcall::Rattach { qid: Qid::read(r)? },
        RERROR => Fcall::Rerror { ename: r.string()? },
        TFLUSH => Fcall::Tflush { oldtag: r.u16()? },
        RFLUSH => Fcall::Rflush,
        TWALK => {
            let fid = r.u32()?;
            let newfid = r.u32()?;
            let n = r.u16()? as usize;
            if n > MAXWELEM {
                return Err(Error::TooManyWalk);
            }
            let wnames = (0..n).map(|_| r.string()).collect::<Result<_, _>>()?;
            Fcall::Twalk {
                fid,
                newfid,
                wnames,
            }
        }
        RWALK => {
            let n = r.u16()? as usize;
            if n > MAXWELEM {
                return Err(Error::TooManyWalk);
            }
            let wqids = (0..n).map(|_| Qid::read(r)).collect::<Result<_, _>>()?;
            Fcall::Rwalk { wqids }
        }
        TOPEN => Fcall::Topen {
            fid: r.u32()?,
            mode: r.u8()?,
        },
        ROPEN => Fcall::Ropen {
            qid: Qid::read(r)?,
            iounit: r.u32()?,
        },
        TCREATE => Fcall::Tcreate {
            fid: r.u32()?,
            name: r.string()?,
            perm: r.u32()?,
            mode: r.u8()?,
        },
        RCREATE => Fcall::Rcreate {
            qid: Qid::read(r)?,
            iounit: r.u32()?,
        },
        TREAD => Fcall::Tread {
            fid: r.u32()?,
            offset: r.u64()?,
            count: r.u32()?,
        },
        RREAD => Fcall::Rread { data: data(r)? },
        TWRITE => Fcall::Twrite {
            fid: r.u32()?,
            offset: r.u64()?,
            data: data(r)?,
        },
        RWRITE => Fcall::Rwrite { count: r.u32()? },
        TCLUNK => Fcall::Tclunk { fid: r.u32()? },
        RCLUNK => Fcall::Rclunk,
        TREMOVE => Fcall::Tremove { fid: r.u32()? },
        RREMOVE => Fcall::Rremove,
        TSTAT => Fcall::Tstat { fid: r.u32()? },
        RSTAT => Fcall::Rstat { stat: stat(r)? },
        TWSTAT => Fcall::Twstat {
            fid: r.u32()?,
            stat: stat(r)?,
        },
        RWSTAT => Fcall::Rwstat,
        other => return Err(Error::BadType(other)),
    })
}

fn encode_body(f: &Fcall<'_>, w: &mut Writer<'_>) -> Result<(), Error> {
    let put_data = |w: &mut Writer<'_>, d: &[u8]| -> Result<(), Error> {
        w.u32(u32::try_from(d.len()).map_err(|_| Error::Overflow)?);
        w.bytes(d);
        Ok(())
    };
    let put_stat = |w: &mut Writer<'_>, s: &[u8]| -> Result<(), Error> {
        w.len16(s.len())?;
        w.bytes(s);
        Ok(())
    };
    match f {
        Fcall::Tversion { msize, version } | Fcall::Rversion { msize, version } => {
            w.u32(*msize);
            w.string(version)?;
        }
        Fcall::Tauth { afid, uname, aname } => {
            w.u32(*afid);
            w.string(uname)?;
            w.string(aname)?;
        }
        Fcall::Rauth { aqid: qid } | Fcall::Rattach { qid } => qid.write(w),
        Fcall::Tattach {
            fid,
            afid,
            uname,
            aname,
        } => {
            w.u32(*fid);
            w.u32(*afid);
            w.string(uname)?;
            w.string(aname)?;
        }
        Fcall::Rerror { ename } => w.string(ename)?,
        Fcall::Tflush { oldtag } => w.u16(*oldtag),
        Fcall::Rflush | Fcall::Rclunk | Fcall::Rremove | Fcall::Rwstat => {}
        Fcall::Twalk {
            fid,
            newfid,
            wnames,
        } => {
            if wnames.len() > MAXWELEM {
                return Err(Error::TooManyWalk);
            }
            w.u32(*fid);
            w.u32(*newfid);
            w.len16(wnames.len())?;
            for n in wnames {
                w.string(n)?;
            }
        }
        Fcall::Rwalk { wqids } => {
            if wqids.len() > MAXWELEM {
                return Err(Error::TooManyWalk);
            }
            w.len16(wqids.len())?;
            for q in wqids {
                q.write(w);
            }
        }
        Fcall::Topen { fid, mode } => {
            w.u32(*fid);
            w.u8(*mode);
        }
        Fcall::Ropen { qid, iounit } | Fcall::Rcreate { qid, iounit } => {
            qid.write(w);
            w.u32(*iounit);
        }
        Fcall::Tcreate {
            fid,
            name,
            perm,
            mode,
        } => {
            w.u32(*fid);
            w.string(name)?;
            w.u32(*perm);
            w.u8(*mode);
        }
        Fcall::Tread { fid, offset, count } => {
            w.u32(*fid);
            w.u64(*offset);
            w.u32(*count);
        }
        Fcall::Rread { data } => put_data(w, data)?,
        Fcall::Twrite { fid, offset, data } => {
            w.u32(*fid);
            w.u64(*offset);
            put_data(w, data)?;
        }
        Fcall::Rwrite { count } => w.u32(*count),
        Fcall::Tclunk { fid } | Fcall::Tremove { fid } | Fcall::Tstat { fid } => w.u32(*fid),
        Fcall::Rstat { stat } => put_stat(w, stat)?,
        Fcall::Twstat { fid, stat } => {
            w.u32(*fid);
            put_stat(w, stat)?;
        }
    }
    Ok(())
}
