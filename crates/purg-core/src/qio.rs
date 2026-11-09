//! Bounded byte queues (Inferno's qio) with blocking reads and writes, the
//! transport under pipes.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use purg_abi::error;
use purg_platform::sync::{Mutex, Rendez};

use crate::block::park;
use crate::error::{Error, Result};

struct State {
    buf: VecDeque<u8>,
    limit: usize,
    hungup: bool,
}

/// A byte queue. Readers block while it is empty, writers while it is full.
/// After [`Queue::hangup`] readers drain what is left and then see end of
/// file, and writers fail with `"i/o on hungup channel"`.
pub struct Queue {
    state: Mutex<State>,
    readers: Rendez,
    writers: Rendez,
    /// Rendezvous registered by `purgatorio.wait` for this queue.
    watchers: Mutex<Vec<Rendez>>,
}

impl Queue {
    pub fn new(limit: usize) -> Queue {
        Queue {
            state: Mutex::new(State {
                buf: VecDeque::new(),
                limit: limit.max(1),
                hungup: false,
            }),
            readers: Rendez::new(),
            writers: Rendez::new(),
            watchers: Mutex::new(Vec::new()),
        }
    }

    fn notify(&self) {
        self.readers.wakeup();
        self.writers.wakeup();
        for w in self.watchers.lock().iter() {
            w.wakeup();
        }
    }

    /// Wakes `w` when data arrives, the queue drains, or it is hung up.
    pub fn watch(&self, w: &Rendez) {
        self.watchers.lock().push(w.clone());
    }

    pub fn unwatch(&self, w: &Rendez) {
        self.watchers.lock().retain(|x| !x.same(w));
    }

    pub fn hungup(&self) -> bool {
        self.state.lock().hungup
    }

    /// A writer would not block.
    pub fn writable(&self) -> bool {
        let s = self.state.lock();
        s.hungup || s.buf.len() < s.limit
    }

    /// A reader would not block.
    pub fn readable(&self) -> bool {
        let s = self.state.lock();
        s.hungup || !s.buf.is_empty()
    }

    /// Reads at least one byte, or returns 0 at end of file. A killed prog
    /// gets [`error::EINTR`](purg_abi::error::EINTR) instead of blocking.
    pub fn read(&self, out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            park(&self.readers, || {
                let s = self.state.lock();
                !s.buf.is_empty() || s.hungup
            })?;
            let (n, hungup) = {
                let mut s = self.state.lock();
                let n = out.len().min(s.buf.len());
                for (o, b) in out.iter_mut().zip(s.buf.drain(..n)) {
                    *o = b;
                }
                (n, s.hungup)
            };
            if n > 0 {
                self.notify();
                return Ok(n);
            }
            // Empty: end of file once hung up; otherwise another reader got
            // there first, so wait again.
            if hungup {
                return Ok(0);
            }
        }
    }

    /// Writes all of `data`, blocking while the queue is full.
    pub fn write(&self, data: &[u8]) -> Result<usize> {
        let mut done = 0;
        while done < data.len() {
            if let Err(e) = park(&self.writers, || {
                let s = self.state.lock();
                s.buf.len() < s.limit || s.hungup
            }) {
                return if done == 0 { Err(e) } else { Ok(done) };
            }
            {
                let mut s = self.state.lock();
                if s.hungup {
                    return Err(Error::new(error::EHUNGUP));
                }
                let n = (s.limit.saturating_sub(s.buf.len())).min(data.len() - done);
                s.buf.extend(&data[done..done + n]);
                done += n;
            }
            self.notify();
        }
        Ok(done)
    }

    /// Marks the queue hung up and wakes everyone.
    pub fn hangup(&self) {
        self.state.lock().hungup = true;
        self.notify();
    }

    pub fn len(&self) -> usize {
        self.state.lock().buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
