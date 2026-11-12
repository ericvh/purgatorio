//! The hosted side of Purgatorio: the host interface (`host`, the only
//! place allowed to depend on the target OS), `#U` (`devfs`), the emu's
//! [`purg_core::Platform`] (`platform`), the window for `emu -g`
//! (`window`) and the Lethe server (`lethed`).
//! The `emu` and `lethed` binaries are built on it.

pub mod devfs;
pub mod host;
pub mod lethed;
pub mod platform;
pub mod window;
