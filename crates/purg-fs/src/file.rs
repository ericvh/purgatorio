//! A disk over a host file or any `Read + Write + Seek` (the `std`
//! feature): what `xtask` and the `disk/` commands work on.

use std::io::{Read, Seek, SeekFrom, Write};

use alloc::borrow::Cow;
use alloc::string::ToString;

use crate::{Disk, Result};

/// A disk image in a file, in units of `sector` bytes.
pub struct FileDisk<F> {
    file: F,
    size: u64,
    sector: usize,
    read_only: bool,
}

fn io(e: std::io::Error) -> Cow<'static, str> {
    Cow::Owned(e.to_string())
}

impl<F: Read + Write + Seek> FileDisk<F> {
    /// The disk is the first `size` bytes of `file`.
    pub fn new(file: F, size: u64, sector: usize) -> FileDisk<F> {
        FileDisk {
            file,
            size,
            sector,
            read_only: false,
        }
    }

    pub fn set_read_only(&mut self, ro: bool) {
        self.read_only = ro;
    }

    pub fn into_inner(self) -> F {
        self.file
    }
}

impl<F: Read + Write + Seek> Disk for FileDisk<F> {
    fn sector(&self) -> usize {
        self.sector
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off.saturating_add(buf.len() as u64) > self.size {
            return Err(Cow::Borrowed(purg_abi::error::EIO));
        }
        self.file.seek(SeekFrom::Start(off)).map_err(io)?;
        // A sparse image may be shorter than its size: the rest reads as
        // zeros.
        let mut done = 0;
        while done < buf.len() {
            match self.file.read(&mut buf[done..]).map_err(io)? {
                0 => {
                    buf[done..].fill(0);
                    break;
                }
                n => done += n,
            }
        }
        Ok(())
    }

    fn write(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Cow::Borrowed(purg_abi::error::EPERM));
        }
        if off.saturating_add(buf.len() as u64) > self.size {
            return Err(Cow::Borrowed(purg_abi::error::EIO));
        }
        self.file.seek(SeekFrom::Start(off)).map_err(io)?;
        self.file.write_all(buf).map_err(io)
    }

    fn flush(&mut self) -> Result<()> {
        self.file.flush().map_err(io)
    }

    fn read_only(&self) -> bool {
        self.read_only
    }
}
