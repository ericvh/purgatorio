//! Kernel channels: bounded queues of byte messages plus transferred
//! capabilities, shared by progs the way file descriptors are. `alt` waits
//! on several of them at once.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;
use purg_platform::sync::{Mutex, Rendez};

use crate::block::{interrupted, park};
use crate::chan::Chan;
use crate::error::{Error, Result};

/// How many channel handles one prog may hold.
const NCHAN: usize = 256;

/// A capability moved with a message: a kernel channel or an open file.
pub enum Cap {
    Chan(Arc<KChan>),
    Fd(Arc<Chan>),
}

/// One message.
pub struct KMsg {
    pub bytes: Vec<u8>,
    pub caps: Vec<Cap>,
}

struct State {
    q: VecDeque<KMsg>,
    /// Threads currently inside [`KChan::recv`], for a zero-capacity rendezvous.
    receivers: u32,
    closed: bool,
}

/// A kernel channel. Capacity `0` is a rendezvous: a send waits until a
/// receive is in progress. A larger capacity buffers that many messages.
pub struct KChan {
    cap: usize,
    state: Mutex<State>,
    send_r: Rendez,
    recv_r: Rendez,
    alts: Mutex<Vec<Rendez>>,
}

impl KChan {
    pub fn new(cap: usize) -> KChan {
        KChan {
            cap,
            state: Mutex::new(State {
                q: VecDeque::new(),
                receivers: 0,
                closed: false,
            }),
            send_r: Rendez::new(),
            recv_r: Rendez::new(),
            alts: Mutex::new(Vec::new()),
        }
    }

    fn notify(&self) {
        self.send_r.wakeup();
        self.recv_r.wakeup();
        for w in self.alts.lock().iter() {
            w.wakeup();
        }
    }

    pub fn watch(&self, w: &Rendez) {
        self.alts.lock().push(w.clone());
    }

    pub fn unwatch(&self, w: &Rendez) {
        self.alts.lock().retain(|x| !x.same(w));
    }

    /// Marks the channel closed and wakes everyone. Further sends fail;
    /// receives drain what is left and then see end.
    pub fn close(&self) {
        self.state.lock().closed = true;
        self.notify();
    }

    pub fn closed(&self) -> bool {
        self.state.lock().closed
    }

    fn can_send_locked(cap: usize, s: &State) -> bool {
        s.closed || s.q.len() < cap || (cap == 0 && s.receivers > 0)
    }

    pub fn can_send(&self) -> bool {
        let s = self.state.lock();
        Self::can_send_locked(self.cap, &s)
    }

    pub fn can_recv(&self) -> bool {
        let s = self.state.lock();
        s.closed || !s.q.is_empty()
    }

    /// Sends `msg`, blocking while the channel is full.
    pub fn send(&self, msg: KMsg) -> Result<()> {
        loop {
            park(&self.send_r, || {
                let s = self.state.lock();
                Self::can_send_locked(self.cap, &s)
            })?;
            let mut s = self.state.lock();
            if s.closed {
                return Err(Error::new(error::EHUNGUP));
            }
            if !Self::can_send_locked(self.cap, &s) {
                continue;
            }
            s.q.push_back(msg);
            drop(s);
            self.notify();
            return Ok(());
        }
    }

    /// Receives one message. `Ok(None)` is a closed, empty channel.
    pub fn recv(&self) -> Result<Option<KMsg>> {
        {
            let mut s = self.state.lock();
            s.receivers += 1;
        }
        self.notify();
        let result = self.recv_inner();
        {
            let mut s = self.state.lock();
            s.receivers = s.receivers.saturating_sub(1);
        }
        self.notify();
        result
    }

    fn recv_inner(&self) -> Result<Option<KMsg>> {
        loop {
            park(&self.recv_r, || {
                let s = self.state.lock();
                s.closed || !s.q.is_empty()
            })?;
            let mut s = self.state.lock();
            if let Some(m) = s.q.pop_front() {
                drop(s);
                self.notify();
                return Ok(Some(m));
            }
            if s.closed {
                return Ok(None);
            }
        }
    }

    /// Takes a queued message without blocking. `Ok(None)` means it would block.
    pub fn try_recv(&self) -> Result<Option<KMsg>> {
        let mut s = self.state.lock();
        if let Some(m) = s.q.pop_front() {
            drop(s);
            self.notify();
            return Ok(Some(m));
        }
        if s.closed {
            return Err(Error::new(error::EHUNGUP));
        }
        Ok(None)
    }

    /// Queues `msg` without blocking. `Ok(None)` means it was queued;
    /// `Ok(Some(msg))` means it would block and the message is returned.
    pub fn try_send(&self, msg: KMsg) -> Result<Option<KMsg>> {
        let mut s = self.state.lock();
        if s.closed {
            return Err(Error::new(error::EHUNGUP));
        }
        if !Self::can_send_locked(self.cap, &s) {
            return Ok(Some(msg));
        }
        s.q.push_back(msg);
        drop(s);
        self.notify();
        Ok(None)
    }

    /// Queues `msg` without blocking, past the channel's capacity if need
    /// be, but with no more than `cap + extra` messages queued in all.
    /// Returns whether it was queued. For notices a sender must not wait
    /// to give, such as a file2chan close event.
    pub fn send_over(&self, msg: KMsg, extra: usize) -> bool {
        let mut s = self.state.lock();
        if s.closed || s.q.len() >= self.cap.saturating_add(extra) {
            return false;
        }
        s.q.push_back(msg);
        drop(s);
        self.notify();
        true
    }
}

impl Drop for KChan {
    fn drop(&mut self) {
        self.state.lock().closed = true;
        self.notify();
    }
}

/// One arm of [`alt`].
pub struct AltOp {
    pub chan: Arc<KChan>,
    pub send: Option<KMsg>,
    pub got: Option<KMsg>,
}

/// Completes the first arm that can proceed, in index order. `block` waits;
/// otherwise `Ok(None)` means none was ready. A killed prog gets
/// [`error::EINTR`](purg_abi::error::EINTR).
pub fn alt(ops: &mut [AltOp], block: bool) -> Result<Option<usize>> {
    let wait = Rendez::new();
    for op in ops.iter() {
        op.chan.watch(&wait);
    }
    let result = alt_inner(ops, block, &wait);
    for op in ops.iter() {
        op.chan.unwatch(&wait);
    }
    result
}

fn alt_inner(ops: &mut [AltOp], block: bool, wait: &Rendez) -> Result<Option<usize>> {
    loop {
        for (i, op) in ops.iter_mut().enumerate() {
            if op.send.is_some() {
                let msg = op.send.take().expect("send arm");
                match op.chan.try_send(msg) {
                    Ok(None) => return Ok(Some(i)),
                    Ok(Some(msg)) => op.send = Some(msg),
                    Err(e) => return Err(e),
                }
            } else {
                match op.chan.try_recv() {
                    Ok(Some(m)) => {
                        op.got = Some(m);
                        return Ok(Some(i));
                    }
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        if !block {
            return Ok(None);
        }
        if interrupted() {
            return Err(Error::new(error::EINTR));
        }
        park(wait, || interrupted() || ops.iter().any(ready))?;
    }
}

fn ready(op: &AltOp) -> bool {
    if op.send.is_some() {
        op.chan.can_send()
    } else {
        op.chan.can_recv()
    }
}

/// A prog's channel-handle table.
pub struct Kgrp {
    tab: Mutex<Vec<Option<Arc<KChan>>>>,
}

impl Kgrp {
    pub fn new() -> Kgrp {
        Kgrp {
            tab: Mutex::new(Vec::new()),
        }
    }

    pub fn alloc(&self, c: Arc<KChan>) -> Result<i32> {
        let mut tab = self.tab.lock();
        let h = match tab.iter().position(Option::is_none) {
            Some(i) => i,
            None if tab.len() < NCHAN => {
                tab.push(None);
                tab.len() - 1
            }
            None => return Err(Error::new(error::ENOFD)),
        };
        tab[h] = Some(c);
        Ok(h as i32)
    }

    pub fn get(&self, h: i32) -> Result<Arc<KChan>> {
        let tab = self.tab.lock();
        usize::try_from(h)
            .ok()
            .and_then(|i| tab.get(i))
            .and_then(Option::clone)
            .ok_or(Error::new(error::EBADFD))
    }

    pub fn close(&self, h: i32) -> Result<Arc<KChan>> {
        let mut tab = self.tab.lock();
        let slot = usize::try_from(h)
            .ok()
            .and_then(|i| tab.get_mut(i))
            .filter(|s| s.is_some())
            .ok_or(Error::new(error::EBADFD))?;
        let old = slot.take().expect("checked");
        while matches!(tab.last(), Some(None)) {
            tab.pop();
        }
        Ok(old)
    }
}

impl Drop for Kgrp {
    fn drop(&mut self) {
        for c in self.tab.lock().drain(..).flatten() {
            c.close();
        }
    }
}

impl Default for Kgrp {
    fn default() -> Kgrp {
        Kgrp::new()
    }
}
