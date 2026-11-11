//! The kernel without the `p9` feature: no virtio-9p transport, no `#9`
//! and nothing on `/n/host`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_core::Dev;

use super::System;

/// There is no transport.
pub type Host = core::convert::Infallible;

/// There is no `#9`.
pub enum Dev9p {}

pub fn find(_fdt: &purg_aarch64::dtb::Fdt<'_>) -> Option<Host> {
    None
}

pub fn open(host: Host) -> Option<Arc<Dev9p>> {
    match host {}
}

pub fn devs(_dev: Option<&Arc<Dev9p>>) -> Vec<Arc<dyn Dev>> {
    Vec::new()
}

impl System {
    pub fn mount_host9p(&self, _dev: Option<&Dev9p>) -> Option<bool> {
        None
    }

    pub fn host9p_tests(&self, _mounted: Option<bool>, _dev: Option<&Dev9p>) -> usize {
        0
    }
}
