//! The kernel without the `net` feature: no virtio-net driver and no
//! smoltcp. `#I` is still in the device table and fails with `network
//! unreachable`, as on any platform without a network stack.

use purg_aarch64::dtb::{BootInfo, Fdt};
use purg_aarch64::platform::MetalPlatform;

use super::System;

/// There is no stack.
pub type Stack = ();

/// There are no network checks.
#[derive(Clone, Copy, Default)]
pub struct Checks;

pub fn open(_fdt: &Fdt<'_>, _info: &BootInfo<'_>) -> (Stack, Checks) {
    ((), Checks)
}

pub fn attach(platform: MetalPlatform, (): Stack) -> MetalPlatform {
    platform
}

impl System {
    pub fn net_tests(&self, _checks: &Checks) -> usize {
        0
    }
}
