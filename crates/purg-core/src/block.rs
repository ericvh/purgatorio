//! Sleep until a condition holds, and wake early when the current prog is
//! killed or interrupted.
//!
//! An interrupt (`docs/abi.md`, "Interrupts") reaches only a prog that
//! survives interrupts; it is pending until the prog next blocks here,
//! and that call ends with [`error::EINTR`], taking it.

use purg_abi::error;
use purg_platform::sync::{Rendez, current_note};

use crate::error::{Error, Result};

/// Whether the prog running on this thread has been killed.
pub fn interrupted() -> bool {
    current_note().is_some_and(|n| n.killed())
}

/// [`error::EINTR`] if the prog running on this thread has been killed or
/// has an interrupt pending, which this takes.
pub fn check() -> Result<()> {
    match current_note() {
        Some(n) if n.take_interrupt() || n.killed() => Err(Error::new(error::EINTR)),
        _ => Ok(()),
    }
}

/// Sleeps on `r` until `cond` returns true. `cond` may run more than once
/// and must not itself sleep. A killed prog gets [`error::EINTR`], and so
/// does an interrupted one, even if `cond` is true. The caller's location
/// is what [`purg_platform::sync::Note::waiting_at`] reports meanwhile.
#[track_caller]
pub fn park(r: &Rendez, mut cond: impl FnMut() -> bool) -> Result<()> {
    let Some(n) = current_note() else {
        r.sleep(&mut cond);
        return Ok(());
    };
    let ok = n.sleep_interruptible(r, &mut cond);
    if n.take_interrupt() {
        return Err(Error::new(error::EINTR));
    }
    if ok || cond() {
        Ok(())
    } else {
        Err(Error::new(error::EINTR))
    }
}

/// Like [`park`], but gives up after `timeout_ms`. `timeout_ms < 0` waits
/// without a limit; `0` does not sleep. Returns whether `cond` is true.
#[track_caller]
pub fn park_timeout(r: &Rendez, mut cond: impl FnMut() -> bool, timeout_ms: i32) -> Result<bool> {
    if timeout_ms < 0 {
        park(r, cond)?;
        return Ok(true);
    }
    let note = current_note();
    let intr = || note.as_ref().is_some_and(|n| n.interrupted());
    if !intr() && cond() {
        return Ok(true);
    }
    check()?;
    if timeout_ms == 0 {
        return Ok(false);
    }
    let ns = u64::from(timeout_ms as u32).saturating_mul(1_000_000);
    if let Some(n) = &note {
        n.arm(r);
    }
    let _hit = r.tsleep(|| cond() || interrupted() || intr(), ns);
    if let Some(n) = &note {
        n.disarm();
    }
    if !intr() && cond() {
        Ok(true)
    } else {
        check()?;
        Ok(false)
    }
}
