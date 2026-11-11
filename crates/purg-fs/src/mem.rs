//! A disk in RAM, which can also play a crash: writes since the last flush
//! are pending, as in a drive's write cache, and a crash keeps an
//! arbitrary subset of them (and an arbitrary prefix of the write it hit),
//! which is what a power cut can leave on a real disk.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::{Disk, Result};

/// A RAM disk.
pub struct MemDisk {
    durable: Vec<u8>,
    /// Written but not flushed, by sector.
    pending: BTreeMap<u64, Vec<u8>>,
    sector: usize,
    read_only: bool,
    /// Writes until the crash, if one is set.
    crash_in: Option<usize>,
    seed: u64,
    crashed: Option<Vec<u8>>,
    /// Write and flush requests so far.
    pub writes: usize,
    pub flushes: usize,
}

impl MemDisk {
    /// A zeroed disk of `size` bytes in units of `sector`.
    pub fn new(size: usize, sector: usize) -> MemDisk {
        MemDisk::from_image(vec![0u8; size], sector)
    }

    /// A disk holding `image`.
    pub fn from_image(image: Vec<u8>, sector: usize) -> MemDisk {
        MemDisk {
            durable: image,
            pending: BTreeMap::new(),
            sector,
            read_only: false,
            crash_in: None,
            seed: 0,
            crashed: None,
            writes: 0,
            flushes: 0,
        }
    }

    pub fn set_read_only(&mut self, ro: bool) {
        self.read_only = ro;
    }

    /// What a clean shutdown leaves: every write.
    pub fn image(&self) -> Vec<u8> {
        let mut img = self.durable.clone();
        for (s, data) in &self.pending {
            let at = *s as usize * self.sector;
            img[at..at + data.len()].copy_from_slice(data);
        }
        img
    }

    /// Crashes the disk during the `n`th write from now (0: the next).
    /// `seed` picks which pending writes and how much of that one land.
    /// From then on writes and flushes are ignored, and [`MemDisk::crashed`]
    /// is what the disk held.
    pub fn crash_after(&mut self, n: usize, seed: u64) {
        self.crash_in = Some(n);
        self.seed = seed | 1;
    }

    pub fn crashed(&self) -> Option<&[u8]> {
        self.crashed.as_deref()
    }

    fn rand(&mut self) -> u64 {
        // xorshift64
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed = x;
        x
    }

    fn crash(&mut self) {
        let mut img = self.durable.clone();
        let pending = core::mem::take(&mut self.pending);
        for (s, data) in &pending {
            if self.rand().is_multiple_of(2) {
                let at = *s as usize * self.sector;
                img[at..at + data.len()].copy_from_slice(data);
            }
        }
        self.pending = pending;
        self.crashed = Some(img);
    }

    fn range(&self, off: u64, len: usize) -> Result<usize> {
        let off = usize::try_from(off).map_err(|_| Cow::Borrowed(purg_abi::error::EIO))?;
        if !off.is_multiple_of(self.sector)
            || !len.is_multiple_of(self.sector)
            || off.checked_add(len).is_none_or(|e| e > self.durable.len())
        {
            return Err(Cow::Borrowed(purg_abi::error::EIO));
        }
        Ok(off)
    }
}

impl Disk for MemDisk {
    fn sector(&self) -> usize {
        self.sector
    }

    fn size(&self) -> u64 {
        self.durable.len() as u64
    }

    fn read(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let at = self.range(off, buf.len())?;
        buf.copy_from_slice(&self.durable[at..at + buf.len()]);
        let first = (at / self.sector) as u64;
        let last = first + (buf.len() / self.sector) as u64;
        for (s, data) in self.pending.range(first..last) {
            let o = (*s - first) as usize * self.sector;
            buf[o..o + data.len()].copy_from_slice(data);
        }
        Ok(())
    }

    fn write(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Cow::Borrowed(purg_abi::error::EPERM));
        }
        let at = self.range(off, buf.len())?;
        self.writes += 1;
        if self.crashed.is_some() {
            return Ok(());
        }
        let first = (at / self.sector) as u64;
        let mut n = buf.len() / self.sector;
        let crash = match self.crash_in {
            Some(0) => {
                // The write is cut short.
                n = (self.rand() % (n as u64 + 1)) as usize;
                true
            }
            Some(k) => {
                self.crash_in = Some(k - 1);
                false
            }
            None => false,
        };
        for (i, chunk) in buf.chunks(self.sector).take(n).enumerate() {
            self.pending.insert(first + i as u64, chunk.to_vec());
        }
        if crash {
            self.crash();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.flushes += 1;
        if self.crashed.is_some() {
            return Ok(());
        }
        for (s, data) in core::mem::take(&mut self.pending) {
            let at = s as usize * self.sector;
            self.durable[at..at + data.len()].copy_from_slice(&data);
        }
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.read_only
    }
}
