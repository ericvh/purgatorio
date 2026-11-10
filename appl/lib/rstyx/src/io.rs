//! File descriptor helpers for the two ends of the protocol.

use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

use purgatorio::Error;
use purgatorio::abi::mode::OREAD;

/// Reads and writes a descriptor this prog does not own as a `File`.
pub struct Fd(pub i32);

impl Fd {
    fn file(&self) -> ManuallyDrop<File> {
        // SAFETY: the descriptor stays open while the `Fd` is in use, and
        // ManuallyDrop keeps the File from closing it.
        ManuallyDrop::new(unsafe { File::from_raw_fd(self.0) })
    }
}

impl Read for Fd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file().read(buf)
    }
}

impl Write for Fd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One read of at most `max` bytes. End of file and an error are both an
/// empty result: either way the stream is over.
pub fn read_some(fd: i32, max: usize) -> Vec<u8> {
    let mut buf = vec![0u8; max];
    let n = Fd(fd).read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    buf
}

/// The argument that makes a command a pump: see [`pump`].
pub const PUMP: &str = "--pump";

/// Copies fd 0 to fd 1, each piece as it arrives, until end of file or an
/// error. A blocking read (a connection, a console) made by a pump can be
/// waited for on the pipe the pump writes: `cat` would do, but for the
/// buffer std puts in front of standard output.
pub fn pump() {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match Fd(0).read(&mut buf) {
            Ok(n) if n > 0 && Fd(1).write_all(&buf[..n]).is_ok() => {}
            _ => return,
        }
    }
}

/// `purgatorio.wait` on fds that would read without blocking (pipes, and
/// a `/prog/n/wait` file). `Ok(None)` is a timeout; a negative timeout
/// waits for ever.
pub fn wait_readable(fds: &[i32], timeout_ms: i32) -> Result<Option<usize>, Error> {
    let ops: Vec<i32> = fds.iter().flat_map(|&fd| [0, fd]).collect();
    let before = Instant::now();
    // SAFETY: `ops` holds `fds.len()` pairs of i32, as the call expects.
    let r = unsafe { purgatorio::sys::wait(ops.as_ptr(), fds.len(), timeout_ms) };
    if r >= 0 {
        return Ok(Some(r as usize));
    }
    // A timeout and an error both return -1; only an error comes back early.
    if timeout_ms >= 0 && before.elapsed() >= Duration::from_millis(timeout_ms as u64) {
        return Ok(None);
    }
    let e = Error::last();
    if timeout_ms == 0 || e.0.is_empty() {
        Ok(None)
    } else {
        Err(e)
    }
}

/// Asks the kernel to kill prog `pid`.
pub fn kill(pid: u32) {
    let _ = std::fs::write(format!("/prog/{pid}/ctl"), "kill");
}

/// This prog's children, as they exit.
pub struct Children {
    wait: Option<File>,
}

impl Children {
    /// Opens this prog's wait file. Call it before starting a child.
    pub fn new() -> Children {
        let path = format!("/prog/{}/wait", purgatorio::pid());
        Children {
            wait: purgatorio::open(&path, OREAD).ok().map(File::from),
        }
    }

    /// Waits until `n` children have exited, or for `ms` milliseconds.
    pub fn reap(&mut self, n: usize, ms: u64) {
        let Some(wait) = self.wait.as_mut() else {
            return;
        };
        let end = Instant::now() + Duration::from_millis(ms);
        let mut buf = [0u8; 4096];
        for _ in 0..n {
            let left = end.saturating_duration_since(Instant::now()).as_millis();
            match wait_readable(&[wait.as_raw_fd()], left as i32) {
                Ok(Some(_)) if wait.read(&mut buf).is_ok() => {}
                _ => return,
            }
        }
    }
}

impl Default for Children {
    fn default() -> Children {
        Children::new()
    }
}
