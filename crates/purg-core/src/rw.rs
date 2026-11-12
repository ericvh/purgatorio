//! Reading and writing a whole buffer through a device, and framing 9P
//! messages on a byte stream.

use alloc::vec::Vec;

use purg_abi::error;
use purg_auth::record::{self, Opener, Sealer};
use purg_platform::sync::Mutex;

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

/// Message buffers kept for reuse by one connection's reader (issue
/// #179). A buffer handed back keeps the bytes of the message it held,
/// so the next message of no greater size is read into it without first
/// being zeroed. Every byte of a buffer [`Pool::take`] gives is
/// overwritten by the read before the message is returned: what an
/// earlier message left is never seen.
pub struct Pool {
    free: Mutex<Vec<Vec<u8>>>,
}

/// Buffers a pool keeps: a reader and a few requests in flight.
const POOL_KEEP: usize = 4;

/// Messages shorter than this are allocated as before: zeroing them costs
/// less than the pool's lock.
const POOL_MIN: usize = 1024;

impl Pool {
    pub fn new() -> Pool {
        Pool {
            free: Mutex::new(Vec::new()),
        }
    }

    /// A buffer of `n` bytes, which may hold an earlier message's bytes.
    /// The smallest kept buffer that is long enough is used, so that the
    /// long ones stay long; else the longest, which is grown.
    fn take(&self, n: usize) -> Vec<u8> {
        if n < POOL_MIN {
            return alloc::vec![0u8; n];
        }
        let mut free = self.free.lock();
        let fits = (0..free.len())
            .filter(|&i| free[i].len() >= n)
            .min_by_key(|&i| free[i].len());
        let pick = fits.or_else(|| (0..free.len()).max_by_key(|&i| free[i].len()));
        let mut b = pick.map(|i| free.swap_remove(i)).unwrap_or_default();
        drop(free);
        if b.len() >= n {
            b.truncate(n);
        } else {
            b.resize(n, 0);
        }
        b
    }

    /// Hands `buf` back for a later message.
    pub fn put(&self, buf: Vec<u8>) {
        if buf.capacity() < POOL_MIN {
            return;
        }
        let mut free = self.free.lock();
        if free.len() < POOL_KEEP {
            free.push(buf);
        }
    }
}

impl Default for Pool {
    fn default() -> Pool {
        Pool::new()
    }
}

/// Reads one 9P message whose `size[4]` is at most `msize`.
pub fn read_msg(c: &Chan, msize: u32) -> Result<Vec<u8>> {
    read_msg_in(c, msize, None)
}

/// [`read_msg`], into a buffer from `pool` when one is given.
pub fn read_msg_in(c: &Chan, msize: u32, pool: Option<&Pool>) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    read_full(c, &mut hdr)?;
    let n = u32::from_le_bytes(hdr) as usize;
    if n < 7 || n > msize as usize {
        return Err(Error::new(error::EIO));
    }
    let mut buf = take(pool, n);
    buf[..4].copy_from_slice(&hdr);
    if let Err(e) = read_full(c, &mut buf[4..]) {
        give_back(pool, buf);
        return Err(e);
    }
    Ok(buf)
}

fn take(pool: Option<&Pool>, n: usize) -> Vec<u8> {
    match pool {
        Some(p) => p.take(n),
        None => alloc::vec![0u8; n],
    }
}

fn give_back(pool: Option<&Pool>, buf: Vec<u8>) {
    if let Some(p) = pool {
        p.put(buf);
    }
}

/// Reads one record of the session layer (`purg_auth::record`) and
/// returns the 9P message it carries, whose `size[4]` is at most `msize`.
/// A record that does not open fails with the record layer's error.
pub fn read_sealed(c: &Chan, opener: &mut Opener, msize: u32) -> Result<Vec<u8>> {
    read_sealed_in(c, opener, msize, None)
}

/// [`read_sealed`], into a buffer from `pool` when one is given.
pub fn read_sealed_in(
    c: &Chan,
    opener: &mut Opener,
    msize: u32,
    pool: Option<&Pool>,
) -> Result<Vec<u8>> {
    let mut hdr = [0u8; record::HEADER];
    read_full(c, &mut hdr)?;
    let n = record::size(hdr, msize as usize).map_err(Error::new)?;
    // The rest of the record, opened where it lands.
    let mut msg = take(pool, n - record::HEADER);
    let opened =
        read_full(c, &mut msg).and_then(|()| opener.open_body(hdr, &mut msg).map_err(Error::new));
    let size = msg
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes);
    match opened {
        Ok(()) if msg.len() >= 7 && size == Some(msg.len() as u32) => Ok(msg),
        Ok(()) => {
            give_back(pool, msg);
            Err(Error::new(error::EIO))
        }
        Err(e) => {
            give_back(pool, msg);
            Err(e)
        }
    }
}

/// Writes `msg` as one record when `sealer` is given, else as it is.
pub fn write_sealed(c: &Chan, sealer: Option<&mut Sealer>, msg: &[u8]) -> Result<()> {
    match sealer {
        Some(s) => write_full(c, s.seal_buf(msg).map_err(Error::new)?),
        None => write_full(c, msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kept buffer is reused at the size asked for; the smallest that
    /// fits is picked, and one too short is grown.
    #[test]
    fn a_pool_reuses_buffers() {
        let pool = Pool::new();
        let (k, m) = (POOL_MIN, 2 * POOL_MIN);
        pool.put(alloc::vec![1u8; 4 * k]);
        pool.put(alloc::vec![2u8; m]);
        let small = pool.take(k);
        assert_eq!(small, alloc::vec![2u8; k]);
        let big = pool.take(8 * k);
        assert_eq!(big.len(), 8 * k);
        assert_eq!(&big[..4 * k], &alloc::vec![1u8; 4 * k][..]);
        assert!(big[4 * k..].iter().all(|&b| b == 0));
        assert_eq!(pool.take(k), alloc::vec![0u8; k]);
        // Small buffers are neither kept nor taken from the pool.
        pool.put(alloc::vec![3u8; 8]);
        assert!(pool.free.lock().is_empty());
        pool.put(alloc::vec![4u8; m]);
        assert_eq!(pool.take(8), [0u8; 8]);
        for _ in 0..POOL_KEEP + 2 {
            pool.put(alloc::vec![0u8; k]);
        }
        assert_eq!(pool.free.lock().len(), POOL_KEEP);
    }
}
