//! `#r`: the real-time clock, as in Inferno's native kernels.
//!
//! `rtc` reads as the clock's time in seconds since the epoch, a decimal
//! number right-justified in 11 characters and followed by a space (12
//! bytes, no newline). Writing a decimal number sets the clock. The clock
//! is the hardware's (a [`Rtc`]), read afresh each time: setting it does
//! not change `#c/time`, which runs from the clock as it was at boot.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::Rtc;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QDIR: u64 = 0;
const QRTC: u64 = 1;

pub struct DevRtc {
    rtc: Arc<dyn Rtc>,
    user: String,
}

impl DevRtc {
    pub fn new(rtc: Arc<dyn Rtc>, user: String) -> DevRtc {
        DevRtc { rtc, user }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, qtype, perm) = match path {
            QRTC => ("rtc", QTFILE, 0o664),
            _ => ("#r", QTDIR, DMDIR | 0o555),
        };
        Dir {
            dtype: 'r' as u16,
            qid: Qid {
                path,
                vers: 0,
                qtype,
            },
            mode: perm,
            name: String::from(name),
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }
}

/// Parses what a write to `rtc` holds: a decimal number of seconds,
/// optionally surrounded by white space.
pub fn parse_seconds(buf: &[u8]) -> Result<u64> {
    let s = core::str::from_utf8(buf)
        .map_err(|_| Error::new(error::EBADARG))?
        .trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::new(error::EBADARG));
    }
    s.parse().map_err(|_| Error::new(error::EBADARG))
}

impl Dev for DevRtc {
    fn dc(&self) -> char {
        'r'
    }

    fn name(&self) -> &'static str {
        "rtc"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QDIR,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QDIR {
            return Err(Error::new(error::ENOTDIR));
        }
        if name != "rtc" {
            return Err(Error::new(error::ENONEXIST));
        }
        Ok(Walked {
            qid: Qid {
                path: QRTC,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QDIR && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EISDIR));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path != QRTC {
            return Err(Error::new(error::EISDIR));
        }
        let text = format!("{:>11} ", self.rtc.seconds());
        Ok(read_str(text.as_bytes(), buf, offset))
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(alloc::vec![self.dir(QRTC)])
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        if c.qid.path != QRTC {
            return Err(Error::new(error::EISDIR));
        }
        if offset != 0 {
            return Err(Error::new(error::EBADARG));
        }
        self.rtc.set_seconds(parse_seconds(buf)?)?;
        Ok(buf.len())
    }
}
