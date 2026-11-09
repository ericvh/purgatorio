//! Sleep until a condition holds, and wake early when the current prog is
//! killed.

use purg_abi::error;
use purg_platform::sync::{Rendez, current_note};

use crate::error::{Error, Result};

/// Whether the prog running on this thread has been killed.
pub fn interrupted() -> bool {
    current_note().is_some_and(|n| n.killed())
}

/// Sleeps on `r` until `cond` returns true. `cond` may run more than once
/// and must not itself sleep. A killed prog gets [`error::EINTR`].
pub fn park(r: &Rendez, mut cond: impl FnMut() -> bool) -> Result<()> {
    let ok = if let Some(n) = current_note() {
        n.sleep(r, &mut cond)
    } else {
        r.sleep(&mut cond);
        true
    };
    if ok || cond() {
        Ok(())
    } else {
        Err(Error::new(error::EINTR))
    }
}

/// Like [`park`], but gives up after `timeout_ms`. `timeout_ms < 0` waits
/// without a limit; `0` does not sleep. Returns whether `cond` is true.
pub fn park_timeout(r: &Rendez, mut cond: impl FnMut() -> bool, timeout_ms: i32) -> Result<bool> {
    if timeout_ms < 0 {
        park(r, cond)?;
        return Ok(true);
    }
    if cond() {
        return Ok(true);
    }
    if interrupted() {
        return Err(Error::new(error::EINTR));
    }
    if timeout_ms == 0 {
        return Ok(false);
    }
    let ns = u64::from(timeout_ms as u32).saturating_mul(1_000_000);
    let note = current_note();
    if let Some(n) = &note {
        n.arm(r);
    }
    let _hit = r.tsleep(|| cond() || interrupted(), ns);
    if let Some(n) = &note {
        n.disarm();
    }
    if cond() {
        Ok(true)
    } else if interrupted() {
        Err(Error::new(error::EINTR))
    } else {
        Ok(false)
    }
}
