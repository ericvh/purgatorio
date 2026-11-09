use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;
use purg_platform::sync::Mutex;

use crate::chan::Chan;
use crate::error::{Error, Result};

/// Maximum number of file descriptors per Fgrp.
pub const NFD: usize = 1024;

/// A file descriptor table: Inferno's `Fgrp`. Progs share one through an
/// `Arc` unless `pctl` gives them a new or forked table.
#[derive(Default)]
pub struct Fgrp {
    fds: Mutex<Vec<Option<Arc<Chan>>>>,
}

impl Fgrp {
    pub fn new() -> Fgrp {
        Fgrp::default()
    }

    /// A copy sharing every open Chan (`FORKFD`).
    pub fn fork(&self) -> Fgrp {
        Fgrp {
            fds: Mutex::new(self.fds.lock().clone()),
        }
    }

    /// A table holding only `keep`, at the same numbers (`NEWFD`).
    pub fn keep(&self, keep: &[i32]) -> Fgrp {
        let fds = self.fds.lock();
        let mut out = Vec::new();
        for &fd in keep {
            if let Some(c) = usize::try_from(fd)
                .ok()
                .and_then(|i| fds.get(i))
                .cloned()
                .flatten()
            {
                let i = fd as usize;
                if out.len() <= i {
                    out.resize(i + 1, None);
                }
                out[i] = Some(c);
            }
        }
        Fgrp {
            fds: Mutex::new(out),
        }
    }

    /// Installs `c` at the lowest free fd.
    pub fn alloc(&self, c: Arc<Chan>) -> Result<i32> {
        let mut fds = self.fds.lock();
        let fd = match fds.iter().position(Option::is_none) {
            Some(i) => i,
            None if fds.len() < NFD => {
                fds.push(None);
                fds.len() - 1
            }
            None => return Err(Error::new(error::ENOFD)),
        };
        fds[fd] = Some(c);
        Ok(fd as i32)
    }

    /// Installs `c` at `fd`, closing whatever was there.
    pub fn install(&self, fd: i32, c: Arc<Chan>) -> Result<()> {
        let i = usize::try_from(fd)
            .ok()
            .filter(|&i| i < NFD)
            .ok_or(Error::new(error::EBADFD))?;
        let old = {
            let mut fds = self.fds.lock();
            if fds.len() <= i {
                fds.resize(i + 1, None);
            }
            fds[i].replace(c)
        };
        drop(old);
        Ok(())
    }

    pub fn get(&self, fd: i32) -> Result<Arc<Chan>> {
        let fds = self.fds.lock();
        usize::try_from(fd)
            .ok()
            .and_then(|i| fds.get(i))
            .and_then(Option::clone)
            .ok_or(Error::new(error::EBADFD))
    }

    pub fn close(&self, fd: i32) -> Result<()> {
        let old = {
            let mut fds = self.fds.lock();
            let slot = usize::try_from(fd)
                .ok()
                .and_then(|i| fds.get_mut(i))
                .filter(|s| s.is_some())
                .ok_or(Error::new(error::EBADFD))?;
            let old = slot.take();
            while matches!(fds.last(), Some(None)) {
                fds.pop();
            }
            old
        };
        // The Chan may close a device (a pipe end, say) that takes locks of
        // its own; do it outside the table lock.
        drop(old);
        Ok(())
    }

    /// Open fds, lowest first.
    pub fn fds(&self) -> Vec<i32> {
        let fds = self.fds.lock();
        (0..fds.len())
            .filter(|&i| fds[i].is_some())
            .map(|i| i as i32)
            .collect()
    }

    /// Closes every fd.
    pub fn clear(&self) {
        let old = core::mem::take(&mut *self.fds.lock());
        drop(old);
    }
}
