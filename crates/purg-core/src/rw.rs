//! Reading and writing a whole buffer through a device, and framing 9P
//! messages on a byte stream.

use alloc::vec::Vec;

use purg_abi::error;
use purg_auth::record::{self, Opener, Sealer};

use crate::chan::Chan;
use crate::error::{Error, Result};

/// Reads `buf.len()` bytes. A short read at end of file is [`error::EHUNGUP`].
pub fn read_full(c: &Chan, buf: &mut [u8]) -> Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = c.dev.read(c, &mut buf[off..], 0)?;
        if n == 0 {
            return Err(Error::new(error::EHUNGUP));
        }
        off += n;
    }
    Ok(())
}

/// Writes all of `buf`.
pub fn write_full(c: &Chan, buf: &[u8]) -> Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = c.dev.write(c, &buf[off..], 0)?;
        if n == 0 {
            return Err(Error::new(error::EHUNGUP));
        }
        off += n;
    }
    Ok(())
}

/// Reads one 9P message whose `size[4]` is at most `msize`.
pub fn read_msg(c: &Chan, msize: u32) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    read_full(c, &mut hdr)?;
    let n = u32::from_le_bytes(hdr) as usize;
    if n < 7 || n > msize as usize {
        return Err(Error::new(error::EIO));
    }
    let mut buf = alloc::vec![0u8; n];
    buf[..4].copy_from_slice(&hdr);
    read_full(c, &mut buf[4..])?;
    Ok(buf)
}

/// Reads one record of the session layer (`purg_auth::record`) and
/// returns the 9P message it carries, whose `size[4]` is at most `msize`.
/// A record that does not open fails with the record layer's error.
pub fn read_sealed(c: &Chan, opener: &mut Opener, msize: u32) -> Result<Vec<u8>> {
    let mut hdr = [0u8; record::HEADER];
    read_full(c, &mut hdr)?;
    let n = record::size(hdr, msize as usize).map_err(Error::new)?;
    let mut rec = alloc::vec![0u8; n];
    rec[..record::HEADER].copy_from_slice(&hdr);
    read_full(c, &mut rec[record::HEADER..])?;
    let msg = opener.open(rec).map_err(Error::new)?;
    let size = msg
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes);
    if msg.len() < 7 || size != Some(msg.len() as u32) {
        return Err(Error::new(error::EIO));
    }
    Ok(msg)
}

/// Writes `msg` as one record when `sealer` is given, else as it is.
pub fn write_sealed(c: &Chan, sealer: Option<&mut Sealer>, msg: &[u8]) -> Result<()> {
    match sealer {
        Some(s) => write_full(c, &s.seal(msg).map_err(Error::new)?),
        None => write_full(c, msg),
    }
}
