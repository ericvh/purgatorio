//! The portable Purgatorio kernel: the code Inferno keeps in `emu/port` and
//! `os/port`. It is `no_std + alloc` and reaches its environment only through
//! [`purg_platform::Platform`], so the hosted emu, the bare-metal probe and the
//! aarch64 kernel all run this same code.
#![no_std]

extern crate alloc;

pub mod auth;
pub mod block;
pub mod chan;
pub mod dev;
pub mod devcmd;
pub mod devcons;
pub mod devdup;
pub mod deveia;
pub mod devenv;
pub mod devfactotum;
pub mod devip;
pub mod devmem;
pub mod devmnt;
pub mod devpipe;
pub mod devprog;
pub mod devram;
pub mod devroot;
pub mod devrtc;
pub mod devsrv;
pub mod devssl;
pub mod error;
pub mod export;
pub mod fgrp;
pub mod initrd;
pub mod kchan;
pub mod linedisc;
pub mod mux;
pub mod names;
pub mod pgrp;
pub mod prog;
pub mod qio;
pub mod rw;
mod sysfile;

use alloc::sync::Arc;
use alloc::vec::Vec;

pub use chan::{Aux, Chan, Member};
pub use dev::{Dev, DevTab, Walked};
pub use devenv::Egrp;
pub use devmnt::DevMnt;
pub use devsrv::DevSrv;
pub use error::{Error, Result};
pub use fgrp::Fgrp;
pub use pgrp::Pgrp;
pub use prog::{Kernel, Proc, Procs, Prog};
pub use purg_platform::Platform;
pub use purg_styx::{Dir, Qid};

/// The devices every Purgatorio system has, plus the `#M` and `#s` instances
/// the kernel keeps so `mount` and `file2chan` can reach them.
pub struct Portable {
    pub devs: Vec<Arc<dyn Dev>>,
    pub srv: Arc<DevSrv>,
    pub mnt: Arc<DevMnt>,
}

/// `#c`, `#e`, `#|`, `#p`, `#s`, `#M`, `#d`, `#%`, `#I`, `#C`, `#¤` and `#D`.
/// Embedders add `#/` and their own devices.
pub fn portable_devices(platform: &Arc<dyn Platform>, procs: &Arc<Procs>) -> Portable {
    let srv = Arc::new(devsrv::DevSrv::new(procs.user()));
    let mnt = Arc::new(devmnt::DevMnt::new(platform.clone()));
    let devs: Vec<Arc<dyn Dev>> = alloc::vec![
        Arc::new(devcons::DevCons::new(platform.clone())),
        Arc::new(devenv::DevEnv::new(procs.clone())),
        Arc::new(devpipe::DevPipe::new(procs.user())),
        Arc::new(devprog::DevProg::new(procs.clone())),
        srv.clone(),
        mnt.clone(),
        Arc::new(devdup::DevDup::new(procs.clone())),
        Arc::new(devmem::DevMem::new(platform.clone())),
        Arc::new(devip::DevIp::new(platform.clone())),
        Arc::new(devcmd::DevCmd::new(platform.clone())),
        Arc::new(devfactotum::DevFactotum::new(platform.clone())),
        Arc::new(devssl::DevSsl::new(platform.clone(), procs.clone())),
    ];
    Portable { devs, srv, mnt }
}
