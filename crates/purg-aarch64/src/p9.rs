//! A blocking 9P transport over [`Virtio9p`]: requests go down as whole
//! messages and replies come back as whole messages, in the order the
//! device answers them.
//!
//! ```ignore
//! let p9 = Host9p::open(&fdt, msize)?;   // after irq::init
//! p9.submit(cookie, &tmsg)?;
//! let (cookie, rmsg) = p9.recv()?;
//! ```
//!
//! [`Host9p::submit`] sleeps while every slot is in flight, and
//! [`Host9p::recv`] sleeps until a reply arrives. Both wait for the
//! device's interrupt (with a [`SAFETY_POLL_MS`] poll in case one is
//! missed), and a killed prog's wait ends with `interrupted`, as in
//! [`crate::net`]. The bytes are not interpreted here: the device speaks
//! whatever dialect the client does, and QEMU's needs 9P2000.u
//! (`purg_styx::dotu`).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use purg_platform::Result;
use purg_platform::sync::{Mutex, Rendez};
use spin::Once;

use crate::dtb::Fdt;
use crate::irq::{self, After};
use crate::virtio::{self, Mmio, Virtio9p, sleep};

/// How long a wait sleeps before looking at the rings again without an
/// interrupt.
pub const SAFETY_POLL_MS: u64 = 100;

/// Bumped by the device's interrupt and when a reply frees a slot.
static EVENTS: AtomicU64 = AtomicU64::new(0);
static WAKE: Once<Rendez> = Once::new();
static IRQ_MMIO: Once<Mmio> = Once::new();

fn device_irq() -> After {
    if let Some(m) = IRQ_MMIO.get() {
        m.ack();
    }
    EVENTS.fetch_add(1, Ordering::Release);
    if let Some(r) = WAKE.get() {
        r.wakeup();
    }
    After::Return
}

/// The virtio-9p device, shared by the threads that use it.
pub struct Host9p {
    dev: Mutex<Virtio9p>,
    tag: alloc::string::String,
    msize: usize,
}

impl Host9p {
    /// Brings up the first virtio-9p device in the DTB with `msize` buffers
    /// and routes its interrupt. Call once, after [`irq::init`].
    pub fn open(fdt: &Fdt<'_>, msize: usize) -> core::result::Result<Host9p, &'static str> {
        let t = virtio::find(fdt, virtio::DEVICE_9P).ok_or("no virtio-9p device")?;
        let dev = Virtio9p::new(&t, msize)?;
        IRQ_MMIO.call_once(|| t.mmio);
        WAKE.call_once(Rendez::new);
        irq::register(t.intid, device_irq);
        Ok(Host9p {
            tag: dev.tag().into(),
            msize,
            dev: Mutex::new(dev),
        })
    }

    /// The device's mount tag.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The largest message each way.
    pub fn msize(&self) -> usize {
        self.msize
    }

    fn wake() -> &'static Rendez {
        WAKE.get().expect("Host9p::open ran")
    }

    /// Sleeps until [`EVENTS`] moves past `seen`.
    fn wait(seen: u64) -> Result<()> {
        sleep(
            Self::wake(),
            || EVENTS.load(Ordering::Acquire) != seen,
            Some(SAFETY_POLL_MS),
        )
        .map(drop)
    }

    /// Sends the message `req`; its reply comes back from [`Host9p::recv`]
    /// with `cookie`.
    pub fn submit(&self, cookie: u64, req: &[u8]) -> Result<()> {
        loop {
            // Read before looking, so an event in between is not missed.
            let seen = EVENTS.load(Ordering::Acquire);
            if self
                .dev
                .lock()
                .submit(req, cookie)
                .map_err(alloc::borrow::Cow::Borrowed)?
            {
                return Ok(());
            }
            Self::wait(seen)?;
        }
    }

    /// Waits for the next reply; returns it with its request's cookie.
    pub fn recv(&self) -> Result<(u64, Vec<u8>)> {
        loop {
            let seen = EVENTS.load(Ordering::Acquire);
            let reply = self.dev.lock().pop();
            if let Some(r) = reply {
                // A slot is free: wake a submit waiting for one.
                Self::poke();
                return Ok(r);
            }
            Self::wait(seen)?;
        }
    }

    /// Wakes every waiter to look again.
    fn poke() {
        EVENTS.fetch_add(1, Ordering::Release);
        if let Some(r) = WAKE.get() {
            r.wakeup();
        }
    }
}
