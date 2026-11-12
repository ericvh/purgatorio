//! The portable Purgatorio kernel: the code Inferno keeps in `emu/port` and
//! `os/port`. It is `no_std + alloc` and reaches its environment only through
//! [`purg_platform::Platform`], so the hosted emu, the bare-metal probe and the
//! aarch64 kernel all run this same code.
#![no_std]

extern crate alloc;

pub mod auth;
pub mod block;
pub mod chan;
pub mod debug;
pub mod dev;
pub mod devcmd;
pub mod devcons;
#[cfg(feature = "draw")]
pub mod devdraw;
pub mod devdup;
pub mod deveia;
pub mod devenv;
pub mod devfactotum;
pub mod devip;
#[cfg(feature = "lethe")]
pub mod devlethe;
pub mod devmem;
pub mod devmnt;
pub mod devpipe;
#[cfg(feature = "draw")]
pub mod devpointer;
pub mod devprog;
pub mod devram;
pub mod devroot;
pub mod devrtc;
pub mod devsrv;
#[cfg(feature = "tls")]
pub mod devssl;
pub mod error;
pub mod export;
pub mod fgrp;
pub mod initrd;
pub mod kchan;
pub mod linedisc;
pub mod modsig;
pub mod mux;
pub mod names;
pub mod pgrp;
pub mod prog;
pub mod qio;
pub mod rw;
mod sysfile;
pub mod workq;

use alloc::sync::Arc;
use alloc::vec::Vec;

pub use chan::{Aux, Chan, Member};
pub use dev::{Dev, DevTab, WalkEnd, Walked, WalkedNames};
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

/// `#c`, `#e`, `#|`, `#p`, `#s`, `#M`, `#d`, `#%`, `#I`, `#C`, `#¤`, with the
/// `tls` feature `#D`, with the `lethe` feature `#λ`, and with the `draw`
/// feature and a platform screen `#i` and `#m`. Embedders add `#/` and
/// their own devices.
pub fn portable_devices(platform: &Arc<dyn Platform>, procs: &Arc<Procs>) -> Portable {
    let srv = Arc::new(devsrv::DevSrv::new(procs.clone()));
    let mnt = Arc::new(devmnt::DevMnt::new(platform.clone()));
    #[cfg_attr(
        not(any(feature = "tls", feature = "lethe", feature = "draw")),
        allow(unused_mut)
    )]
    let mut devs: Vec<Arc<dyn Dev>> = alloc::vec![
        Arc::new(devcons::DevCons::new(platform.clone(), procs.clone())),
        Arc::new(devenv::DevEnv::new(procs.clone())),
        Arc::new(devpipe::DevPipe::new(procs.user())),
        Arc::new(devprog::DevProg::new(procs.clone())),
        srv.clone(),
        mnt.clone(),
        Arc::new(devdup::DevDup::new(procs.clone())),
        Arc::new(devmem::DevMem::new(platform.clone())),
        Arc::new(devip::DevIp::new(platform.clone())),
        Arc::new(devcmd::DevCmd::new(platform.clone())),
        Arc::new(devfactotum::DevFactotum::new(
            platform.clone(),
            procs.clone(),
        )),
    ];
    #[cfg(feature = "tls")]
    devs.push(Arc::new(devssl::DevSsl::new(
        platform.clone(),
        procs.clone(),
    )));
    #[cfg(feature = "lethe")]
    devs.push(Arc::new(devlethe::DevLethe::new(platform.clone())));
    #[cfg(feature = "draw")]
    if let Some(screen) = platform.screen()
        && let Some(draw) = devdraw::DevDraw::new(screen.clone(), procs.user().into())
    {
        devs.push(Arc::new(draw));
        let p = platform.clone();
        let clock = Arc::new(move || (p.monotonic_ns() / 1_000_000) as u32);
        devs.push(Arc::new(devpointer::DevPointer::new(
            screen,
            procs.user().into(),
            clock,
        )));
    }
    Portable { devs, srv, mnt }
}
