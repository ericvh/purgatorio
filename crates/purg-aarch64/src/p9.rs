//! A blocking 9P transport over [`Virtio9p`]: requests go down as whole
//! messages and replies come back as whole messages, in the order the
//! device answers them.
//!
//! ```ignore
//! let (devs, _errors) = Host9p::open_all(&fdt, msize);   // after irq::init
//! let p9 = &devs[0];
//! p9.submit(cookie, &tmsg)?;
//! let (cookie, rmsg) = p9.recv()?;
//! ```
//!
//! [`Host9p::submit`] sleeps while every slot is in flight, and
//! [`Host9p::recv`] sleeps until a reply arrives. Both wait for the
//! device's interrupt (with a [`SAFETY_POLL_MS`] poll in case one is
//! missed), and a killed prog's wait ends with `interrupted`, as in
//! [`crate::net`]. Every device shares one interrupt handler and one
//! [`Rendez`]: a wake-up makes each waiter look at its own ring. The bytes are not interpreted here: the device speaks
//! whatever dialect the client does, and QEMU's needs 9P2000.u
//! (`purg_styx::dotu`).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use purg_platform::Result;
use purg_platform::sync::{Mutex, Rendez};
use spin::Once;

use crate::dtb::Fdt;
use crate::irq::{self, After};
use crate::virtio::{self, Mmio, Virtio9p, sleep};

/// How long a wait sleeps before looking at the rings again without an
/// interrupt.
pub const SAFETY_POLL_MS: u64 = 100;
/// The most devices the shared interrupt handler serves.
pub const MAX_DEVICES: usize = 4;

/// Bumped by the devices' interrupt and when a reply frees a slot.
static EVENTS: AtomicU64 = AtomicU64::new(0);
static WAKE: Once<Rendez> = Once::new();
/// The devices' registers, as `Mmio` base addresses (0 for none).
static IRQ_MMIO: [AtomicUsize; MAX_DEVICES] = [const { AtomicUsize::new(0) }; MAX_DEVICES];

fn device_irq() -> After {
    for m in &IRQ_MMIO {
        let base = m.load(Ordering::Acquire);
        if base != 0 {
            // SAFETY: only `Host9p::open_all` stores here, a virtio window.
            unsafe { Mmio::new(base) }.ack();
        }
    }
    EVENTS.fetch_add(1, Ordering::Release);
    if let Some(r) = WAKE.get() {
        r.wakeup();
    }
    After::Return
}

/// A virtio-9p device, shared by the threads that use it.
pub struct Host9p {
    dev: Mutex<Virtio9p>,
    tag: alloc::string::String,
    msize: usize,
}

impl Host9p {
    /// Brings up every virtio-9p device in the DTB (up to
    /// [`MAX_DEVICES`]) with `msize` buffers, and routes their interrupts.
    /// Call once, after [`irq::init`]. A device that fails to come up is
    /// reported in the second list and left out.
    pub fn open_all(fdt: &Fdt<'_>, msize: usize) -> (Vec<Host9p>, Vec<&'static str>) {
        let mut devs = Vec::new();
        let mut errors = Vec::new();
        WAKE.call_once(Rendez::new);
        for t in virtio::find_all(fdt, virtio::DEVICE_9P).take(MAX_DEVICES) {
            match Virtio9p::new(&t, msize) {
                Ok(dev) => {
                    IRQ_MMIO[devs.len()].store(t.mmio.base(), Ordering::Release);
                    irq::register(t.intid, device_irq);
                    devs.push(Host9p {
                        tag: dev.tag().into(),
                        msize,
                        dev: Mutex::new(dev),
                    });
                }
                Err(e) => errors.push(e),
            }
        }
        (devs, errors)
    }

    /// The device's mount tag.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The largest message each way.
    pub fn msize(&self) -> usize {
        self.msize
    }

    /// Requests the device has not answered.
    pub fn in_flight(&self) -> usize {
        self.dev.lock().in_flight()
    }

    fn wake() -> &'static Rendez {
        WAKE.get().expect("Host9p::open_all ran")
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
