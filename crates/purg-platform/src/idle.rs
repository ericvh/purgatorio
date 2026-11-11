//! When an SMP kernel's CPUs may wait for an interrupt (`wfi`), and which
//! ones a waker must interrupt so that none sleeps through a change.
//!
//! The kernel scheduler has no blocked state: a thread whose condition is
//! false calls [`Sched::block`](crate::sync::Sched::block), which counts
//! the block and runs another thread. Only once more blocks than there are
//! threads that could still look have happened in a row, with no event in
//! between, may the CPU wait (see the scheduler's docs for why the count is
//! enough). Any event ([`IdleGate::wake`]) restarts the count.
//!
//! On one CPU an interrupt is the only event a waiting CPU can miss, and
//! `wfi` returns for one even with interrupts masked. On several CPUs the
//! event may come from another CPU, which must then send an IPI to every
//! CPU that is waiting or about to. The count and the set of waiting CPUs
//! share one atomic word, so the handshake is two read-modify-writes of
//! the same location, which are totally ordered:
//!
//! - a waker resets the count and, in the same operation, reads the set of
//!   waiting CPUs;
//! - a CPU about to wait adds itself to the set and, in the same
//!   operation, reads the count, and waits only if it is still past the
//!   threshold.
//!
//! Whichever comes second sees the first: the waker sees the CPU in the
//! set (and interrupts it; the interrupt stays pending until the `wfi`),
//! or the CPU sees the reset (and does not wait).
//! `crates/purg-core/tests/loom.rs` checks this.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};

/// The most CPUs a gate tracks.
pub const MAX_CPUS: usize = 16;

/// The low [`MAX_CPUS`] bits are the waiting set; the count is above.
const COUNT: u64 = 1 << MAX_CPUS;
const SET: u64 = COUNT - 1;

pub struct IdleGate {
    /// Bit `n` is set while CPU `n` is waiting or deciding to; the bits
    /// above count blocks in a row with no event in between.
    state: AtomicU64,
}

impl IdleGate {
    #[cfg(not(loom))]
    pub const fn new() -> IdleGate {
        IdleGate {
            state: AtomicU64::new(0),
        }
    }

    #[cfg(loom)]
    pub fn new() -> IdleGate {
        IdleGate {
            state: AtomicU64::new(0),
        }
    }

    /// Counts one block; returns the count, this one included.
    pub fn block(&self) -> usize {
        ((self.state.fetch_add(COUNT, Ordering::AcqRel) >> MAX_CPUS) + 1) as usize
    }

    /// Something changed: restarts the count. Returns the CPUs that are
    /// waiting or about to, which the caller must interrupt.
    #[must_use]
    pub fn wake(&self) -> u64 {
        self.state.fetch_and(SET, Ordering::AcqRel) & SET
    }

    /// Restarts the count without asking who waits: for an event that only
    /// this CPU can have missed, such as its own timer interrupt.
    pub fn local_event(&self) {
        let _ = self.wake();
    }

    /// CPU `cpu` would wait if the count is past `threshold`. Announces it
    /// and returns whether it may: if it may not, it has already left.
    /// After a `true`, the caller waits (with interrupts masked, so a wake's
    /// IPI stays pending) and then calls [`IdleGate::leave`].
    pub fn enter(&self, cpu: usize, threshold: usize) -> bool {
        let before = self.state.fetch_or(1 << cpu, Ordering::AcqRel);
        if (before >> MAX_CPUS) as usize > threshold {
            true
        } else {
            self.leave(cpu);
            false
        }
    }

    /// Adds `cpu` to the waiting set, for a wait whose condition the caller
    /// checks itself afterwards under a lock (an idle CPU looking at its
    /// run queue): whoever changes it under that lock and then calls
    /// [`IdleGate::waiting`] sees the CPU.
    pub fn announce(&self, cpu: usize) {
        self.state.fetch_or(1 << cpu, Ordering::AcqRel);
    }

    /// Takes `cpu` out of the waiting set.
    pub fn leave(&self, cpu: usize) {
        self.state.fetch_and(!(1 << cpu), Ordering::AcqRel);
    }

    /// The CPUs that are waiting or about to, for a waker that changed
    /// something they check themselves (a run queue that gained a thread).
    pub fn waiting(&self) -> u64 {
        self.state.load(Ordering::Acquire) & SET
    }
}

#[cfg(not(loom))]
impl Default for IdleGate {
    fn default() -> IdleGate {
        IdleGate::new()
    }
}
