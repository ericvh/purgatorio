//! The kernel without the `blk` feature: no virtio-blk driver, no `#S`,
//! and no file system on `/n/disk`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_core::{Dev, Platform};

use super::System;

/// There are no disks.
pub struct Blks;

/// Nor a file system.
#[derive(Default)]
pub struct Disks;

pub fn find(_fdt: &purg_aarch64::dtb::Fdt<'_>) -> Blks {
    Blks
}

pub fn open(_blks: Blks, _platform: &Arc<dyn Platform>) -> Disks {
    Disks
}

pub fn devs(_disks: &Disks, _user: &str) -> Vec<Arc<dyn Dev>> {
    Vec::new()
}

impl System {
    pub fn bind_disk(&self, _disks: &Disks) -> Option<bool> {
        None
    }

    pub fn disk_tests(&self, _disks: &Disks, _bound: Option<bool>) -> usize {
        0
    }

    pub fn disk_persist(&self, _disks: &Disks, _bound: Option<bool>) -> usize {
        self.say("kernel: FAIL built without the blk feature");
        1
    }
}
