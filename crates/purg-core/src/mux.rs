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
}

/// Outstanding Styx tags for one mounted connection.
pub struct Mux {
    next: core::sync::atomic::AtomicU16,
    slots: Mutex<BTreeMap<u16, Slot>>,
    wake: Rendez,
}

impl Mux {
    pub fn new() -> Mux {
        Mux {
            next: core::sync::atomic::AtomicU16::new(0),
            slots: Mutex::new(BTreeMap::new()),
            wake: Rendez::new(),
        }
    }

    /// Allocates a tag and records an empty slot for it.
    pub fn begin(&self) -> Result<u16> {
        let mut slots = self.slots.lock();
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
            },
        );
        Ok(tag)
    }

    /// Blocks until a reply is delivered or the tag is flushed. A killed prog
    /// gets [`error::EINTR`](purg_abi::error::EINTR) and the slot stays, so
    /// the caller can [`Mux::flush`] it.
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

    /// Fails every outstanding tag, as when the connection closes.
    pub fn fail_all(&self) {
        let mut slots = self.slots.lock();
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
