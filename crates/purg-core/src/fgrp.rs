use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;

use crate::chan::Chan;
use crate::error::{Error, Result};

/// Maximum number of file descriptors per Fgrp.
pub const NFD: usize = 1024;

/// A file descriptor table: Inferno's `Fgrp`.
#[derive(Default)]
pub struct Fgrp {
    fds: Vec<Option<Arc<Chan>>>,
}

impl Fgrp {
    pub fn new() -> Fgrp {
        Fgrp::default()
    }

    /// Installs `c` at the lowest free fd.
    pub fn alloc(&mut self, c: Arc<Chan>) -> Result<i32> {
        let fd = match self.fds.iter().position(Option::is_none) {
            Some(i) => i,
            None if self.fds.len() < NFD => {
                self.fds.push(None);
                self.fds.len() - 1
            }
            None => return Err(Error::new(error::ENOFD)),
        };
        self.fds[fd] = Some(c);
        Ok(fd as i32)
    }

    pub fn get(&self, fd: i32) -> Result<&Arc<Chan>> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.fds.get(i))
            .and_then(Option::as_ref)
            .ok_or(Error::new(error::EBADFD))
    }

    pub fn close(&mut self, fd: i32) -> Result<()> {
        let slot = usize::try_from(fd)
            .ok()
            .and_then(|i| self.fds.get_mut(i))
            .filter(|s| s.is_some())
            .ok_or(Error::new(error::EBADFD))?;
        *slot = None;
        while matches!(self.fds.last(), Some(None)) {
            self.fds.pop();
        }
        Ok(())
    }
}
