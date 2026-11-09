//! The portable Purgatorio kernel: the code Inferno keeps in `emu/port` and
//! `os/port`. It is `no_std + alloc` and reaches its environment only through
//! [`purg_platform::Platform`], so the hosted emu, the bare-metal probe and the
//! aarch64 kernel all run this same code.
#![no_std]

extern crate alloc;

pub mod chan;
pub mod dev;
pub mod devcons;
pub mod devenv;
pub mod devpipe;
pub mod devprog;
pub mod devroot;
pub mod error;
pub mod fgrp;
pub mod names;
pub mod pgrp;
pub mod prog;
pub mod qio;
mod sysfile;

use alloc::sync::Arc;
use alloc::vec::Vec;

pub use chan::{Aux, Chan, Member};
pub use dev::{Dev, DevTab, Walked};
pub use devenv::Egrp;
pub use error::{Error, Result};
pub use fgrp::Fgrp;
pub use pgrp::Pgrp;
pub use prog::{Kernel, Proc, Procs, Prog};
pub use purg_platform::Platform;
pub use purg_styx::{Dir, Qid};

/// The devices every Purgatorio system has, whatever it runs on: `#c`,
/// `#e`, `#|` and `#p`. Embedders add `#/` and their own devices.
pub fn portable_devices(platform: &Arc<dyn Platform>, procs: &Arc<Procs>) -> Vec<Arc<dyn Dev>> {
    alloc::vec![
        Arc::new(devcons::DevCons::new(platform.clone())),
        Arc::new(devenv::DevEnv::new(procs.clone())),
        Arc::new(devpipe::DevPipe::new(procs.user())),
        Arc::new(devprog::DevProg::new(procs.clone())),
    ]
}
