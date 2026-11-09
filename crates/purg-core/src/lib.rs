//! The portable Purgatorio kernel: the code Inferno keeps in `emu/port` and
//! `os/port`. It is `no_std + alloc` and reaches its environment only through
//! [`purg_platform::Platform`], so the hosted emu, the bare-metal probe and the
//! aarch64 kernel all run this same code.
#![no_std]

extern crate alloc;

pub mod chan;
pub mod dev;
pub mod devcons;
pub mod devroot;
pub mod error;
pub mod fgrp;
pub mod names;
pub mod pgrp;
pub mod prog;
mod sysfile;

pub use chan::{Aux, Chan};
pub use dev::{Dev, DevTab, Walked};
pub use error::{Error, Result};
pub use fgrp::Fgrp;
pub use pgrp::Pgrp;
pub use prog::{Kernel, Prog};
pub use purg_platform::Platform;
pub use purg_styx::{Dir, Qid};
