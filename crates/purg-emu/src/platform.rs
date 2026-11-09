//! [`Platform`] for the hosted emu.

use std::cell::Cell;
use std::io::{Read, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use purg_platform::{Console, Platform, Result};

use crate::host;

thread_local! {
    static CURRENT_PID: Cell<u32> = const { Cell::new(0) };
}

/// The emu's console: the process's stdin and stdout.
struct StdConsole;

impl Console for StdConsole {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        std::io::stdin()
            .read(buf)
            .map_err(|e| host::error_string(&e).into())
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let mut out = std::io::stdout().lock();
        out.write_all(buf)
            .and_then(|()| out.flush())
            .map_err(|e| host::error_string(&e))?;
        Ok(buf.len())
    }
}

pub struct Emu {
    start: Instant,
    console: StdConsole,
    user: String,
    sysname: String,
}

impl Emu {
    pub fn new() -> Emu {
        Emu {
            start: Instant::now(),
            console: StdConsole,
            user: host::user_name(),
            sysname: host::host_name(),
        }
    }
}

impl Platform for Emu {
    fn now_ns(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    }

    fn monotonic_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    fn entropy(&self, buf: &mut [u8]) {
        host::entropy(buf);
    }

    fn console(&self) -> &dyn Console {
        &self.console
    }

    fn user(&self) -> String {
        self.user.clone()
    }

    fn sysname(&self) -> String {
        self.sysname.clone()
    }

    fn current_pid(&self) -> u32 {
        CURRENT_PID.with(Cell::get)
    }

    fn set_current_pid(&self, pid: u32) {
        CURRENT_PID.with(|c| c.set(pid));
    }
}
