use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_platform::Platform;

use crate::dev::DevTab;
use crate::error::Error;
use crate::fgrp::Fgrp;
use crate::pgrp::Pgrp;

/// Kernel-wide state shared by all progs.
pub struct Kernel {
    pub platform: Arc<dyn Platform>,
    pub devtab: Arc<DevTab>,
    next_pid: AtomicU32,
}

impl Kernel {
    pub fn new(platform: Arc<dyn Platform>, devtab: Arc<DevTab>) -> Arc<Kernel> {
        Arc::new(Kernel {
            platform,
            devtab,
            next_pid: AtomicU32::new(1),
        })
    }

    pub fn new_pid(&self) -> u32 {
        self.next_pid.fetch_add(1, Ordering::Relaxed)
    }
}

/// A prog: the schedulable unit, with its namespace, fds and errstr.
pub struct Prog {
    pub pid: u32,
    pub kernel: Arc<Kernel>,
    pub pgrp: Arc<Pgrp>,
    pub fgrp: Fgrp,
    /// The current directory, as a cleaned absolute name.
    pub dot: String,
    pub errstr: String,
}

impl Prog {
    pub fn new(kernel: Arc<Kernel>, pgrp: Arc<Pgrp>) -> Prog {
        Prog {
            pid: kernel.new_pid(),
            kernel,
            pgrp,
            fgrp: Fgrp::new(),
            dot: String::from("/"),
            errstr: String::new(),
        }
    }

    /// Records `e` as the prog's errstr (Inferno's `kwerrstr`).
    pub fn set_error(&mut self, e: &Error) {
        self.errstr.clear();
        self.errstr.push_str(e.as_str());
    }

    pub fn platform(&self) -> &dyn Platform {
        &*self.kernel.platform
    }
}
