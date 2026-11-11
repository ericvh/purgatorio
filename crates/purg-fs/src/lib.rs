//! `purgfs`: Purgatorio's native disk file system (K3, decision D7), shared
//! by the kernel, the `disk/` tools and `xtask`.
//!
//! The design is Plan 9's kfs with a journal: a superblock, a journal, a
//! free map (one bit per block), then data. There are no inodes: a file
//! is its 256-byte entry in its parent directory's blocks (the root's is
//! in block 0), holding its name, owner, group, mode, times, qid and
//! block pointers (16 direct, one single and one double indirect).
//! Metadata changes are atomic through the journal ([`Fs::op`]): a crash
//! never leaves the metadata half changed, and [`Fs::check`] verifies
//! that. `docs/fs.md` has the layout, the crash rules and the limits.
//!
//! The crate is `no_std` with `alloc`, and works on a [`Disk`]: a RAM
//! image ([`mem::MemDisk`]), a host file (`std` feature,
//! [`file::FileDisk`]) or, in the kernel, a virtio-blk device. With the
//! `dev` feature, [`dev::DevFs`] serves a mounted file system as a
//! `purg_core` device.
#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

mod check;
pub mod crc;
#[cfg(feature = "dev")]
pub mod dev;
#[cfg(feature = "std")]
pub mod file;
mod fs;
pub mod layout;
pub mod mem;

use alloc::borrow::Cow;

pub use check::Report;
pub use fs::{
    ATIME_SLACK, CACHE_BLOCKS, Format, Fs, Loc, Mount, R, Ref, STEP, Usage, W, WStat, X, perm_ok,
};
pub use layout::Dentry;

/// An Inferno error string.
pub type Error = Cow<'static, str>;
pub type Result<T> = core::result::Result<T, Error>;

/// A block device, addressed in bytes. Every request [`Fs`] makes is a
/// whole number of its blocks at a block-aligned offset, and its blocks are
/// a multiple of [`Disk::sector`].
pub trait Disk {
    /// The device's unit of transfer, in bytes.
    fn sector(&self) -> usize;
    /// The device's size in bytes.
    fn size(&self) -> u64;
    fn read(&mut self, off: u64, buf: &mut [u8]) -> Result<()>;
    fn write(&mut self, off: u64, buf: &[u8]) -> Result<()>;
    /// Returns once everything written before is on stable storage.
    fn flush(&mut self) -> Result<()>;
    fn read_only(&self) -> bool {
        false
    }
}

impl<D: Disk + ?Sized> Disk for &mut D {
    fn sector(&self) -> usize {
        (**self).sector()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn read(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read(off, buf)
    }
    fn write(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        (**self).write(off, buf)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
}
