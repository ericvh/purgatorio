//! A wait queue: the sleepers of one [`Rendez`](crate::sync::Rendez) on a
//! kernel whose scheduler can park threads.
//!
//! The scheduler is seen through [`Parker`]: a thread names itself, parks
//! and is unparked, with the semantics of a permit. An unpark of a thread
//! that is not parked makes its next park return at once, and a park may
//! also return for no reason, so every sleeper rechecks its condition.
//!
//! A sleeper puts itself on the queue and only then looks at its condition
//! again, and a waker changes the state before it empties the queue. So
//! either the waker finds the sleeper on the queue and unparks it, or the
//! sleeper queued itself after the waker emptied the queue, which is after
//! the change, and sees it. `crates/purg-core/tests/loom.rs` checks this
//! with a parker made of a mutex and a condition variable.

use alloc::vec::Vec;

#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(not(loom))]
use spin::Mutex;

/// What a [`WaitQueue`] needs from the scheduler.
pub trait Parker {
    /// Names the calling thread, for [`Parker::unpark`].
    fn current(&self) -> usize;
    /// Stops the calling thread until it is unparked or, given a
    /// deadline in monotonic nanoseconds, until that passes. Returns at
    /// once if the thread was unparked since its last park. May return
    /// early. Returns whether the deadline has passed.
    fn park(&self, deadline_ns: Option<u64>) -> bool;
    /// Lets thread `id` run: now if it is parked, or through its next
    /// park if it is not. Must be safe in an interrupt handler, and for
    /// an `id` whose thread has gone.
    fn unpark(&self, id: usize);
    /// Masks interrupts on this CPU and returns what
    /// [`Parker::unmask`] needs to put them back: the queue is locked
    /// only between the two, so an interrupt handler never finds it
    /// locked by the thread it interrupted.
    fn mask(&self) -> usize {
        0
    }
    fn unmask(&self, _saved: usize) {}
}

/// The threads sleeping on one rendezvous.
pub struct WaitQueue {
    waiters: Mutex<Vec<usize>>,
}

impl Default for WaitQueue {
    fn default() -> WaitQueue {
        WaitQueue::new()
    }
}

impl WaitQueue {
    pub fn new() -> WaitQueue {
        WaitQueue {
            waiters: Mutex::new(Vec::new()),
        }
    }

    fn with<R>(&self, p: &dyn Parker, f: impl FnOnce(&mut Vec<usize>) -> R) -> R {
        let saved = p.mask();
        #[cfg(loom)]
        let r = f(&mut self.waiters.lock().unwrap());
        #[cfg(not(loom))]
        let r = f(&mut self.waiters.lock());
        p.unmask(saved);
        r
    }

    /// One round of a sleep: unless `cond` is already true, queues the
    /// calling thread, looks again, and parks until a [`WaitQueue::wake`]
    /// or the deadline. The caller loops while its condition is false.
    /// Returns whether it parked until the deadline.
    pub fn wait(
        &self,
        p: &dyn Parker,
        cond: &mut dyn FnMut() -> bool,
        deadline_ns: Option<u64>,
    ) -> bool {
        let me = p.current();
        self.with(p, |w| w.push(me));
        let late = !cond() && p.park(deadline_ns);
        self.with(p, |w| {
            if let Some(i) = w.iter().position(|&t| t == me) {
                w.swap_remove(i);
            }
        });
        late
    }

    /// Unparks every thread on the queue, so each looks at its condition
    /// again. Call after changing what they wait for.
    pub fn wake(&self, p: &dyn Parker) {
        // The list is freed outside the lock.
        let woken = self.with(p, core::mem::take);
        for t in woken {
            p.unpark(t);
        }
    }
}
