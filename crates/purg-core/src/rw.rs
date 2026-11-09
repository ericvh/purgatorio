//! Reading and writing a whole buffer through a device, and framing 9P
//! messages on a byte stream.

use alloc::vec::Vec;

use purg_abi::error;

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
