//! Two small hand-off protocols `export` is built on: a work queue served
//! by a pool of kprocs that grows on demand, and a queue of output drained
//! by whichever thread gets there first.
//!
//! Neither holds a lock while it sleeps or while the caller's code runs,
//! so both are usable on the kernel, where a held facade lock disables
//! preemption. Both have loom models (`tests/loom.rs`).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use purg_platform::sync::{Mutex, Rendez};

struct Work<T> {
    queue: VecDeque<T>,
    /// Workers asleep in [`WorkQ::take`], or woken and not yet looking.
    idle: usize,
    /// Workers started and not yet told to exit.
    workers: usize,
    closed: bool,
}

/// What [`WorkQ::put`] asks of its caller.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    /// A worker will take the item.
    Queued,
    /// The caller must start one more worker, which is already counted. If
    /// it cannot, it calls [`WorkQ::unstarted`].
    Start,
}

/// Items handed from a producer to a pool of workers.
///
/// The pool grows when an item arrives and no worker is free to take it,
/// up to `max` workers; with every worker busy, further items wait. A
/// worker that finds nothing to do while `keep` others are already idle is
/// told to exit, so a burst of blocked requests does not leave its kprocs
/// (and their stacks) behind.
pub struct WorkQ<T> {
    state: Mutex<Work<T>>,
    max: usize,
    keep: usize,
    /// Workers sleep here for an item or for [`WorkQ::close`].
    work: Rendez,
    /// [`WorkQ::join`] sleeps here for the last worker.
    done: Rendez,
}

impl<T> WorkQ<T> {
    pub fn new(max: usize, keep: usize) -> WorkQ<T> {
        WorkQ {
            state: Mutex::new(Work {
                queue: VecDeque::new(),
                idle: 0,
                workers: 0,
                closed: false,
            }),
            max: max.max(1),
            keep,
            work: Rendez::new(),
            done: Rendez::new(),
        }
    }

    /// Queues `item` for a worker.
    pub fn put(&self, item: T) -> Put {
        let start = {
            let mut s = self.state.lock();
            s.queue.push_back(item);
            // Each idle worker takes one item. An item beyond that has
            // nobody coming for it until a busy worker finishes, which may
            // be never: it needs a worker of its own.
            let start = s.queue.len() > s.idle && s.workers < self.max;
            if start {
                s.workers += 1;
            }
            start
        };
        self.work.wakeup();
        if start { Put::Start } else { Put::Queued }
    }

    /// The worker [`Put::Start`] asked for could not be started. Returns
    /// how many workers there are.
    pub fn unstarted(&self) -> usize {
        let left = {
            let mut s = self.state.lock();
            s.workers -= 1;
            s.workers
        };
        self.done.wakeup();
        left
    }

    /// The next item, without waiting. For a producer that has no worker.
    pub fn try_take(&self) -> Option<T> {
        self.state.lock().queue.pop_front()
    }

    /// A worker's loop: the next item, sleeping until there is one. `None`
    /// tells the worker to exit; it is no longer counted.
    pub fn take(&self) -> Option<T> {
        loop {
            {
                let mut s = self.state.lock();
                if let Some(item) = s.queue.pop_front() {
                    return Some(item);
                }
                if s.closed || s.idle >= self.keep {
                    s.workers -= 1;
                    drop(s);
                    self.done.wakeup();
                    return None;
                }
                s.idle += 1;
            }
            self.work.sleep(|| {
                let s = self.state.lock();
                !s.queue.is_empty() || s.closed
            });
            self.state.lock().idle -= 1;
        }
    }

    /// Stops the pool: idle workers exit, busy ones exit when they next
    /// call [`WorkQ::take`]. Returns the items no worker had taken.
    pub fn close(&self) -> Vec<T> {
        let left = {
            let mut s = self.state.lock();
            s.closed = true;
            s.queue.drain(..).collect()
        };
        self.work.wakeup();
        left
    }

    /// Sleeps until every worker has been told to exit.
    pub fn join(&self) {
        self.done.sleep(|| self.state.lock().workers == 0);
    }

    /// Workers started and not yet told to exit.
    pub fn workers(&self) -> usize {
        self.state.lock().workers
    }
}

struct Out<T> {
    queue: VecDeque<T>,
    /// Some thread is in [`OutQ::drain`], between items.
    draining: bool,
}

/// Output from many threads, written by one at a time and in the order it
/// was queued, with no lock held across the write.
///
/// A thread queues its item and then offers to drain. If another thread is
/// already draining, that thread writes the item and this one goes on with
/// its work.
pub struct OutQ<T> {
    state: Mutex<Out<T>>,
}

impl<T> OutQ<T> {
    pub fn new() -> OutQ<T> {
        OutQ {
            state: Mutex::new(Out {
                queue: VecDeque::new(),
                draining: false,
            }),
        }
    }

    /// Queues `item`. It is written by the next [`OutQ::drain`], or by the
    /// one that is running.
    pub fn push(&self, item: T) {
        self.state.lock().queue.push_back(item);
    }

    /// Writes queued items with `write`, oldest first, until none is left,
    /// unless another thread is already doing so. `write` runs with no lock
    /// held and may block.
    pub fn drain(&self, mut write: impl FnMut(T)) {
        {
            let mut s = self.state.lock();
            if s.draining || s.queue.is_empty() {
                return;
            }
            s.draining = true;
        }
        loop {
            let item = {
                let mut s = self.state.lock();
                match s.queue.pop_front() {
                    Some(item) => item,
                    None => {
                        // Under the lock a `push` takes: an item queued
                        // after this is drained by the thread that queued it.
                        s.draining = false;
                        return;
                    }
                }
            };
            write(item);
        }
    }
}

impl<T> Default for OutQ<T> {
    fn default() -> OutQ<T> {
        OutQ::new()
    }
}
