use alloc::string::String;
use alloc::vec::Vec;

use crate::Error;
use crate::wire::{Reader, Writer};

pub const QTDIR: u8 = 0x80;
pub const QTAPPEND: u8 = 0x40;
pub const QTEXCL: u8 = 0x20;
pub const QTAUTH: u8 = 0x08;
pub const QTFILE: u8 = 0x00;

pub const DMDIR: u32 = 0x8000_0000;
pub const DMAPPEND: u32 = 0x4000_0000;
pub const DMEXCL: u32 = 0x2000_0000;
pub const DMAUTH: u32 = 0x0800_0000;

/// Size of a packed [`Dir`] with empty strings: `size[2]` plus the fixed
/// fields plus four `len[2]` string prefixes.
pub const STATFIXLEN: usize = 2 + 2 + 4 + Qid::LEN + 4 + 4 + 4 + 8 + 4 * 2;

/// A server's unique identification of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Qid {
    pub path: u64,
    pub vers: u32,
    pub qtype: u8,
}

impl Qid {
    pub const LEN: usize = 13;

    pub fn is_dir(&self) -> bool {
        self.qtype & QTDIR != 0
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Qid, Error> {
        let qtype = r.u8()?;
        let vers = r.u32()?;
        let path = r.u64()?;
        Ok(Qid { path, vers, qtype })
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) {
        w.u8(self.qtype);
        w.u32(self.vers);
        w.u64(self.path);
    }
}

/// A directory entry, as carried by `Rstat`, `Twstat` and directory reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dir {
    /// Device type (the device character for kernel devices).
    pub dtype: u16,
    /// Device instance.
    pub dev: u32,
    pub qid: Qid,
    pub mode: u32,
    pub atime: u32,
    pub mtime: u32,
    pub length: u64,
    pub name: String,
    pub uid: String,
    pub gid: String,
    pub muid: String,
}

impl Dir {
    /// The "don't touch" Dir used by `wstat`: every field all-ones or empty.
    pub fn null() -> Dir {
        Dir {
            dtype: !0,
            dev: !0,
            qid: Qid {
                path: !0,
                vers: !0,
                qtype: !0,
            },
            mode: !0,
            atime: !0,
            mtime: !0,
            length: !0,
            ..Dir::default()
        }
    }

    /// Size of the packed entry, including its own `size[2]` field.
    pub fn packed_len(&self) -> usize {
        STATFIXLEN + self.name.len() + self.uid.len() + self.gid.len() + self.muid.len()
    }

    /// Appends the 9P stat encoding (`size[2]` first) to `out`.
    pub fn pack_into(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let body = self.packed_len() - 2;
        let mut w = Writer { buf: out };
        w.len16(body)?;
        w.u16(self.dtype);
        w.u32(self.dev);
        self.qid.write(&mut w);
        w.u32(self.mode);
        w.u32(self.atime);
        w.u32(self.mtime);
        w.u64(self.length);
        w.string(&self.name)?;
        w.string(&self.uid)?;
        w.string(&self.gid)?;
        w.string(&self.muid)?;
        Ok(())
    }

    pub fn pack(&self) -> Result<Vec<u8>, Error> {
        let mut v = Vec::with_capacity(self.packed_len());
        self.pack_into(&mut v)?;
        Ok(v)
    }

    /// Decodes one entry from the front of `buf`, returning it and the
    /// number of bytes consumed.
    pub fn unpack(buf: &[u8]) -> Result<(Dir, usize), Error> {
        let mut r = Reader::new(buf);
        let n = r.u16()? as usize;
        let body = r.bytes(n)?;
        let mut r = Reader::new(body);
        let dir = Dir {
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
        r.finish()?;
        Ok((dir, n + 2))
    }
}

/// Iterates over the packed entries of a directory read.
pub fn dirs(mut buf: &[u8]) -> impl Iterator<Item = Result<Dir, Error>> + '_ {
    core::iter::from_fn(move || {
        if buf.is_empty() {
            return None;
        }
        match Dir::unpack(buf) {
            Ok((d, n)) => {
                buf = &buf[n..];
                Some(Ok(d))
            }
            Err(e) => {
                buf = &[];
                Some(Err(e))
            }
        }
    })
}
