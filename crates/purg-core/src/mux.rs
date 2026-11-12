//! Tag multiplexer for a Styx client (`#M`).
//!
//! Each outstanding request owns a tag. The reader kproc delivers replies by
//! tag. `flush` makes a reply that has not yet arrived invisible to the
//! waiter, which is what `Tflush` requires: a late `R` for a flushed tag must
//! not be reported as the result of the request.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use purg_abi::error;
use purg_platform::sync::{Mutex, Rendez};
use purg_styx::NOTAG;

use crate::block::park;
use crate::error::{Error, Result};

struct Slot {
    msg: Option<Vec<u8>>,
    flushed: bool,
    /// For the tag of a `Tflush` nobody waits for any more: the tag it
    /// flushed, which is free again when the `Rflush` arrives.
    frees: Option<u16>,
}

/// Outstanding Styx tags for one mounted connection.
pub struct Mux {
    next: core::sync::atomic::AtomicU16,
    slots: Mutex<BTreeMap<u16, Slot>>,
    /// Set by [`Mux::fail_all`]: no reply will ever arrive.
    closed: core::sync::atomic::AtomicBool,
    /// Why, when it was not a hangup ([`Mux::fail_because`]).
    why: Mutex<Option<&'static str>>,
    wake: Rendez,
}

impl Mux {
    pub fn new() -> Mux {
        Mux {
            next: core::sync::atomic::AtomicU16::new(0),
            slots: Mutex::new(BTreeMap::new()),
            closed: core::sync::atomic::AtomicBool::new(false),
            why: Mutex::new(None),
            wake: Rendez::new(),
        }
    }

    /// The error for a request on a closed connection.
    fn closed_error(&self) -> Error {
        Error::new(self.why.lock().unwrap_or(error::EHUNGUP))
    }

    /// Allocates a tag and records an empty slot for it. Fails with
    /// [`error::EHUNGUP`] once the connection has closed.
    pub fn begin(&self) -> Result<u16> {
        let mut slots = self.slots.lock();
        if self.closed.load(core::sync::atomic::Ordering::Acquire) {
            drop(slots);
            return Err(self.closed_error());
        }
        if slots.len() >= 64 * 1024 - 1 {
            return Err(Error::new(error::EIO));
        }
        let tag = loop {
            let t = self
                .next
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if t == NOTAG || slots.contains_key(&t) {
                continue;
            }
            break t;
        };
        slots.insert(
            tag,
            Slot {
                msg: None,
                flushed: false,
                frees: None,
            },
        );
        Ok(tag)
    }

    /// Blocks until a reply is delivered or the tag is flushed. A killed prog
    /// gets [`error::EINTR`](purg_abi::error::EINTR) and the slot stays, so
    /// the caller can [`Mux::flush`] it. When the connection closed before
    /// the reply came the error is [`error::EHUNGUP`].
    pub fn wait(&self, tag: u16) -> Result<Vec<u8>> {
        park(&self.wake, || {
            let slots = self.slots.lock();
            slots
                .get(&tag)
                .is_some_and(|s| s.msg.is_some() || s.flushed)
        })?;
        let mut slots = self.slots.lock();
        let Some(slot) = slots.remove(&tag) else {
            return Err(Error::new(error::EIO));
        };
        match slot.msg {
            Some(m) => Ok(m),
            None if self.closed.load(core::sync::atomic::Ordering::Acquire) => {
                drop(slots);
                Err(self.closed_error())
            }
            None => Err(Error::new(error::EINTR)),
        }
    }

    /// Hands `msg` to the waiter for `tag`. Returns false when the tag is
    /// unknown or already flushed, in which case the message is discarded.
    pub fn deliver(&self, tag: u16, msg: Vec<u8>) -> bool {
        let mut slots = self.slots.lock();
        let Some(slot) = slots.get_mut(&tag) else {
            return false;
        };
        if slot.flushed {
            // The `Rflush` of an abandoned `Tflush` ends both tags. Any
            // other flushed tag waits for its own `Rflush`.
            if let Some(old) = slot.frees {
                slots.remove(&tag);
                slots.remove(&old);
            }
            drop(slots);
            self.wake.wakeup();
            return false;
        }
        slot.msg = Some(msg);
        drop(slots);
        self.wake.wakeup();
        true
    }

    /// Makes a reply that has not arrived yet invisible to [`Mux::wait`].
    /// A reply that is already waiting is left for the waiter.
    pub fn flush(&self, tag: u16) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(&tag)
            && slot.msg.is_none()
        {
            slot.flushed = true;
        }
        drop(slots);
        self.wake.wakeup();
    }

    /// Frees `tag`: its request is over and nobody will [`Mux::wait`] for
    /// it. For a flushed tag once its `Rflush` has arrived.
    pub fn forget(&self, tag: u16) {
        self.slots.lock().remove(&tag);
    }

    /// The caller sent a `Tflush` with tag `ftag` for `oldtag` and will not
    /// wait for the `Rflush` (its prog was killed). Both tags stay taken
    /// until the `Rflush` arrives, then both are freed.
    pub fn abandon(&self, ftag: u16, oldtag: u16) {
        let mut slots = self.slots.lock();
        match slots.get_mut(&ftag) {
            Some(slot) if slot.msg.is_none() => {
                slot.flushed = true;
                slot.frees = Some(oldtag);
            }
            _ => {
                slots.remove(&ftag);
                slots.remove(&oldtag);
            }
        }
    }

    /// The caller sent the request on `tag` and will not wait for its
    /// reply (its prog was killed). The tag stays taken until the reply
    /// arrives, which is then dropped, or until the connection closes.
    pub fn discard(&self, tag: u16) {
        let mut slots = self.slots.lock();
        match slots.get_mut(&tag) {
            Some(slot) if slot.msg.is_none() => {
                slot.flushed = true;
                slot.frees = Some(tag);
            }
            _ => drop(slots.remove(&tag)),
        }
    }

    /// Blocks until no tag is in use (every reply has arrived, including
    /// those nobody waits for) or the connection has closed. A killed
    /// prog gets [`error::EINTR`].
    pub fn settle(&self) -> Result<()> {
        park(&self.wake, || {
            self.slots.lock().is_empty() || self.closed.load(core::sync::atomic::Ordering::Acquire)
        })
    }

    /// Tags in use.
    pub fn outstanding(&self) -> usize {
        self.slots.lock().len()
    }

    /// [`Mux::fail_all`], with `why` as the error the waiters get instead
    /// of a hangup. The first reason given is kept.
    pub fn fail_because(&self, why: &'static str) {
        self.why.lock().get_or_insert(why);
        self.fail_all();
    }

    /// Fails every outstanding tag, and every later [`Mux::begin`], as when
    /// the connection closes.
    pub fn fail_all(&self) {
        let mut slots = self.slots.lock();
        self.closed
            .store(true, core::sync::atomic::Ordering::Release);
        for slot in slots.values_mut() {
            if slot.msg.is_none() {
                slot.flushed = true;
            }
        }
        drop(slots);
        self.wake.wakeup();
    }
}

impl Default for Mux {
    fn default() -> Mux {
        Mux::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A request that was interrupted and flushed gives its tag back when
    /// the `Rflush` arrives, whether or not anyone waited for it.
    #[test]
    fn a_flush_frees_its_tags() {
        let mux = Mux::new();
        // The caller waits for the Rflush.
        let old = mux.begin().unwrap();
        mux.flush(old);
        let ftag = mux.begin().unwrap();
        assert!(mux.deliver(ftag, vec![1]));
        assert_eq!(mux.wait(ftag).unwrap(), [1]);
        mux.forget(old);
        assert_eq!(mux.outstanding(), 0);

        // The caller does not wait: the tags are held until the Rflush.
        let old = mux.begin().unwrap();
        mux.flush(old);
        let ftag = mux.begin().unwrap();
        mux.abandon(ftag, old);
        assert_eq!(mux.outstanding(), 2);
        // A late reply to the flushed request changes nothing.
        assert!(!mux.deliver(old, vec![2]));
        assert_eq!(mux.outstanding(), 2);
        assert!(!mux.deliver(ftag, vec![3]));
        assert_eq!(mux.outstanding(), 0);

        // The Rflush came before the caller gave up.
        let old = mux.begin().unwrap();
        mux.flush(old);
        let ftag = mux.begin().unwrap();
        assert!(mux.deliver(ftag, vec![4]));
        mux.abandon(ftag, old);
        assert_eq!(mux.outstanding(), 0);
    }

    /// A request whose reply nobody waits for keeps its tag until the
    /// reply arrives, and the reply is dropped.
    #[test]
    fn a_discarded_request_frees_its_tag_on_reply() {
        let mux = Mux::new();
        let t = mux.begin().unwrap();
        mux.discard(t);
        assert_eq!(mux.outstanding(), 1);
        assert!(!mux.deliver(t, vec![1]));
        assert_eq!(mux.outstanding(), 0);

        // The reply came before the caller gave up.
        let t = mux.begin().unwrap();
        assert!(mux.deliver(t, vec![2]));
        mux.discard(t);
        assert_eq!(mux.outstanding(), 0);
    }
}
