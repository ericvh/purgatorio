//! A virtio-blk driver (QEMU's `-device virtio-blk-device`), blocking and
//! interrupt-driven.
//!
//! ```ignore
//! let disks = Blk::open_all(&fdt);      // after irq::init
//! disks[0].read(sector, &mut buf, Wait::Interruptible)?;
//! disks[0].write(sector, &buf, Wait::Uninterruptible)?;
//! disks[0].flush()?;
//! ```
//!
//! [`VirtioBlk`] is the device: one request queue of [`SLOTS`] slots. Each
//! slot owns three descriptors (a header the device reads, a data buffer,
//! a status byte the device writes) and a bounce buffer of up to
//! [`XFER`] bytes, so a request is a copy in, a kick, and later a copy out.
//! The DMA and cache rules are those of the net and 9P drivers
//! ([`crate::virtio`]): every region is whole cache lines from the heap,
//! cleaned before the device reads it and invalidated before the CPU reads
//! what the device wrote.
//!
//! Sizes: the device counts in 512-byte sectors (the virtio unit, whatever
//! the medium), and reports its logical block size with
//! `VIRTIO_BLK_F_BLK_SIZE` (512 without it). [`Blk`] takes only requests
//! that are whole logical blocks at block-aligned sectors, so a 4 KiB-sector
//! device is never asked for a partial block.
//!
//! Waiting: [`Blk`]'s calls sleep on a [`Rendez`] that the device's
//! interrupt wakes, with a [`SAFETY_POLL_MS`] poll in case one is missed.
//! With [`Wait::Interruptible`] a prog killed while it waits gets
//! `interrupted` at once. Its request stays in flight: the slot is marked
//! abandoned, and whoever next looks at the used ring frees it when the
//! device finishes, so the device writes into a bounce buffer that nobody
//! reads and the queue stays consistent. [`Wait::Uninterruptible`] is for
//! callers (the file system's metadata writes) that must know whether a
//! write happened.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use purg_abi::error;
use purg_platform::Result;
use purg_platform::sync::{Mutex, Rendez};
use spin::Once;

use crate::dtb::Fdt;
use crate::irq::{self, After};
use crate::mmu;
use crate::virtio::{
    self, ACKNOWLEDGE, DESC_NEXT, DESC_WRITE, DRIVER, DRIVER_OK, F_VERSION_1, FEATURES_OK, Mmio,
    REG_CONFIG, REG_QUEUE_AVAIL, REG_QUEUE_DESC, REG_QUEUE_NOTIFY, REG_QUEUE_NUM,
    REG_QUEUE_NUM_MAX, REG_QUEUE_READY, REG_QUEUE_SEL, REG_QUEUE_USED, REG_STATUS, Transport,
    dcache_clean, dcache_inval, dma, fence,
};

/// The virtio sector, in bytes: the unit of a request's position and of
/// the capacity, whatever the device's block size.
pub const SECTOR: u64 = 512;

/// Requests in flight at once.
pub const SLOTS: usize = 4;
/// Descriptors on the queue: three per slot, rounded up to a power of two
/// as a split virtqueue requires.
const DESCS: usize = 16;
/// The most one request moves, before `VIRTIO_BLK_F_SIZE_MAX` and the
/// block size are taken into account.
pub const XFER: usize = 16 << 10;
/// The most disks the shared interrupt handler serves.
pub const MAX_DISKS: usize = 4;
/// How long a wait sleeps before looking at the ring again without an
/// interrupt.
pub const SAFETY_POLL_MS: u64 = 100;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const S_OK: u8 = 0;
const S_UNSUPP: u8 = 2;
/// What the status byte holds until the device writes it.
const S_PENDING: u8 = 0xff;
/// The length of a `GET_ID` serial number.
const ID_LEN: usize = 20;

const F_SIZE_MAX: u64 = 1 << 1;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;

// Configuration space offsets.
const CFG_CAPACITY: usize = 0;
const CFG_SIZE_MAX: usize = 8;
const CFG_BLK_SIZE: usize = 20;

/// A slot's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Free,
    /// Handed to the device.
    Busy,
    /// The device is done; the status byte it wrote.
    Done(u8),
    /// Handed to the device, and its waiter is gone: freed when the device
    /// is done with it.
    Abandoned,
}

/// What a request does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Read,
    Write,
    Flush,
    GetId,
}

/// A virtio block device.
pub struct VirtioBlk {
    mmio: Mmio,
    /// In 512-byte sectors.
    capacity: u64,
    block_size: usize,
    read_only: bool,
    can_flush: bool,
    /// The most one request moves: a whole number of blocks.
    xfer: usize,
    /// The distance between slots' bounce buffers: `xfer` in whole
    /// writeback granules.
    buf_stride: usize,
    /// Each slot's header and status: two cache writeback granules, so
    /// the status the device writes never shares a granule with the
    /// header the CPU wrote.
    meta: usize,
    meta_stride: usize,
    desc: usize,
    avail: usize,
    used: usize,
    bufs: usize,
    avail_idx: u16,
    last_used: u16,
    slots: [Slot; SLOTS],
    /// The data length of each slot's request.
    lens: [usize; SLOTS],
}

impl VirtioBlk {
    /// Resets and brings up the block device at `t`.
    pub fn new(t: &Transport) -> core::result::Result<VirtioBlk, &'static str> {
        let m = t.mmio;
        if m.device_id() != Some(virtio::DEVICE_BLK) {
            return Err("not a virtio-blk device");
        }
        m.write(REG_STATUS, 0);
        while m.read(REG_STATUS) != 0 {
            core::hint::spin_loop();
        }
        m.write(REG_STATUS, ACKNOWLEDGE);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER);
        let offered = virtio::device_features(&m);
        if offered & F_VERSION_1 == 0 {
            return Err("virtio-blk is not a version 1 device");
        }
        let want = (F_VERSION_1 | F_SIZE_MAX | F_RO | F_BLK_SIZE | F_FLUSH) & offered;
        virtio::driver_features(&m, want);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        if m.read(REG_STATUS) & FEATURES_OK == 0 {
            return Err("virtio-blk rejected features");
        }
        let lo = u64::from(m.read(REG_CONFIG + CFG_CAPACITY));
        let hi = u64::from(m.read(REG_CONFIG + CFG_CAPACITY + 4));
        let capacity = lo | (hi << 32);
        let block_size = if want & F_BLK_SIZE != 0 {
            m.read(REG_CONFIG + CFG_BLK_SIZE) as usize
        } else {
            SECTOR as usize
        };
        if block_size < SECTOR as usize || !block_size.is_power_of_two() || block_size > XFER {
            return Err("virtio-blk block size is not a power of two from 512 to 16 KiB");
        }
        let mut xfer = XFER;
        if want & F_SIZE_MAX != 0 {
            let max = m.read(REG_CONFIG + CFG_SIZE_MAX) as usize;
            if max >= block_size {
                xfer = xfer.min(max);
            }
        }
        let xfer = xfer / block_size * block_size;

        m.write(REG_QUEUE_SEL, 0);
        if (m.read(REG_QUEUE_NUM_MAX) as usize) < DESCS {
            return Err("virtio queue is too small");
        }
        let align = crate::cache::dma_align();
        let meta_stride = 2 * align;
        let buf_stride = xfer.next_multiple_of(align);
        let desc = dma(16 * DESCS);
        let avail = dma(4 + 2 * DESCS + 2);
        let used = dma(4 + 8 * DESCS + 2);
        let meta = dma(meta_stride * SLOTS);
        let bufs = dma(buf_stride * SLOTS);
        let set = |lo: usize, va: usize| {
            let pa = mmu::virt_to_phys(va) as u64;
            m.write(lo, pa as u32);
            m.write(lo + 4, (pa >> 32) as u32);
        };
        m.write(REG_QUEUE_NUM, DESCS as u32);
        set(REG_QUEUE_DESC, desc);
        set(REG_QUEUE_AVAIL, avail);
        set(REG_QUEUE_USED, used);
        // The rings were zeroed through the cache; the device reads memory.
        dcache_clean(avail, 4 + 2 * DESCS + 2);
        dcache_clean(used, 4 + 8 * DESCS + 2);
        m.write(REG_QUEUE_READY, 1);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        fence();
        Ok(VirtioBlk {
            mmio: m,
            capacity,
            block_size,
            read_only: want & F_RO != 0,
            can_flush: want & F_FLUSH != 0,
            xfer,
            buf_stride,
            meta,
            meta_stride,
            desc,
            avail,
            used,
            bufs,
            avail_idx: 0,
            last_used: 0,
            slots: [Slot::Free; SLOTS],
            lens: [0; SLOTS],
        })
    }

    /// The capacity in 512-byte sectors.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The logical block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The most one request moves.
    pub fn xfer(&self) -> usize {
        self.xfer
    }

    /// The device's registers, for an interrupt handler's [`Mmio::ack`].
    pub fn mmio(&self) -> Mmio {
        self.mmio
    }

    fn header(&self, slot: usize) -> usize {
        self.meta + slot * self.meta_stride
    }

    fn status(&self, slot: usize) -> usize {
        self.header(slot) + self.meta_stride / 2
    }

    fn buf(&self, slot: usize) -> usize {
        self.bufs + slot * self.buf_stride
    }

    fn write_desc(&self, id: usize, addr: usize, len: usize, flags: u16, next: u16) {
        let d = self.desc + id * 16;
        // SAFETY: `d` is descriptor `id` of this queue's table.
        unsafe {
            write_volatile(d as *mut u64, mmu::virt_to_phys(addr) as u64);
            write_volatile((d + 8) as *mut u32, len as u32);
            write_volatile((d + 12) as *mut u16, flags);
            write_volatile((d + 14) as *mut u16, next);
        }
    }

    /// Hands a request to the device: `op` at `sector`, moving `len` bytes
    /// (`data` for a write). Returns its slot, or `None` when every slot is
    /// in use. The caller has checked `len` against [`VirtioBlk::xfer`].
    pub fn submit(&mut self, op: Op, sector: u64, data: &[u8], len: usize) -> Option<usize> {
        self.reap();
        let slot = self.slots.iter().position(|s| *s == Slot::Free)?;
        let len = len.min(self.xfer);
        let kind = match op {
            Op::Read => T_IN,
            Op::Write => T_OUT,
            Op::Flush => T_FLUSH,
            Op::GetId => T_GET_ID,
        };
        let (hdr, st, buf) = (self.header(slot), self.status(slot), self.buf(slot));
        // SAFETY: the slot is free, so the device is not using its header,
        // status or buffer.
        unsafe {
            write_volatile(hdr as *mut u32, kind);
            write_volatile((hdr + 4) as *mut u32, 0);
            write_volatile((hdr + 8) as *mut u64, sector);
            write_volatile(st as *mut u8, S_PENDING);
            if op == Op::Write {
                core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, len.min(data.len()));
            }
        }
        dcache_clean(hdr, 16);
        // Cleaned, then discarded: no dirty line is written back over the
        // byte the device writes.
        dcache_clean(st, 1);
        dcache_inval(st, 1);
        match op {
            Op::Write => dcache_clean(buf, len),
            Op::Read | Op::GetId => dcache_inval(buf, len),
            Op::Flush => {}
        }
        let head = 3 * slot;
        if op == Op::Flush {
            self.write_desc(head, hdr, 16, DESC_NEXT, (head + 2) as u16);
        } else {
            let flags = if op == Op::Write { 0 } else { DESC_WRITE };
            self.write_desc(head, hdr, 16, DESC_NEXT, (head + 1) as u16);
            self.write_desc(head + 1, buf, len, flags | DESC_NEXT, (head + 2) as u16);
        }
        self.write_desc(head + 2, st, 1, DESC_WRITE, 0);
        let ring = self.avail_idx as usize % DESCS;
        // SAFETY: the entry is in this queue's available ring.
        unsafe { write_volatile((self.avail + 4 + ring * 2) as *mut u16, head as u16) };
        fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: the index field of the available ring.
        unsafe { write_volatile((self.avail + 2) as *mut u16, self.avail_idx) };
        dcache_clean(self.desc, 16 * DESCS);
        dcache_clean(self.avail, 4 + 2 * DESCS + 2);
        fence();
        self.slots[slot] = Slot::Busy;
        self.lens[slot] = len;
        self.mmio.write(REG_QUEUE_NOTIFY, 0);
        Some(slot)
    }

    /// Takes what the device has finished off the used ring: a busy slot
    /// is done, an abandoned one is free again. Returns how many it took.
    pub fn reap(&mut self) -> usize {
        let mut n = 0;
        loop {
            dcache_inval(self.used, 4 + 8 * DESCS + 2);
            // SAFETY: the index field of the used ring, just invalidated.
            let idx = unsafe { read_volatile((self.used + 2) as *const u16) };
            if idx == self.last_used {
                return n;
            }
            let elem = self.used + 4 + (self.last_used as usize % DESCS) * 8;
            // SAFETY: the element is in the used ring.
            let head = unsafe { read_volatile(elem as *const u32) } as usize;
            self.last_used = self.last_used.wrapping_add(1);
            let slot = head / 3;
            if !head.is_multiple_of(3) || slot >= SLOTS {
                continue;
            }
            match self.slots[slot] {
                Slot::Busy => {
                    let st = self.status(slot);
                    dcache_inval(st, 1);
                    // SAFETY: the device wrote the status byte.
                    let status = unsafe { read_volatile(st as *const u8) };
                    self.slots[slot] = Slot::Done(status);
                }
                Slot::Abandoned => self.slots[slot] = Slot::Free,
                Slot::Free | Slot::Done(_) => {}
            }
            n += 1;
        }
    }

    /// The status of `slot`'s request once the device is done with it,
    /// with the data copied into `out` for a read, and the slot freed.
    /// `None` while it is in flight.
    pub fn take(&mut self, slot: usize, out: &mut [u8]) -> Option<u8> {
        let Slot::Done(status) = self.slots[slot] else {
            return None;
        };
        let n = out.len().min(self.lens[slot]);
        if n > 0 {
            let buf = self.buf(slot);
            dcache_inval(buf, n);
            // SAFETY: the device is done with the buffer, and the slot is
            // ours until it is freed below.
            unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, out.as_mut_ptr(), n) };
        }
        self.slots[slot] = Slot::Free;
        Some(status)
    }

    /// Gives up on `slot`'s request: freed now if the device is done with
    /// it, else when it is. Returns whether it was still in flight.
    pub fn abandon(&mut self, slot: usize) -> bool {
        match self.slots[slot] {
            Slot::Busy => {
                self.slots[slot] = Slot::Abandoned;
                true
            }
            Slot::Done(_) => {
                self.slots[slot] = Slot::Free;
                false
            }
            Slot::Free | Slot::Abandoned => false,
        }
    }

    /// Requests the device has not finished, abandoned ones included.
    pub fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, Slot::Busy | Slot::Abandoned))
            .count()
    }

    /// The device's serial number (`serial=` in QEMU), asked for by
    /// polling the ring: for bring-up, before the interrupt is routed.
    /// Empty if the device does not answer `GET_ID`.
    fn serial_polled(&mut self) -> String {
        let Some(slot) = self.submit(Op::GetId, 0, &[], ID_LEN) else {
            return String::new();
        };
        let mut id = [0u8; ID_LEN];
        for _ in 0..50_000_000u32 {
            self.reap();
            if let Some(status) = self.take(slot, &mut id) {
                if status != S_OK {
                    return String::new();
                }
                let n = id.iter().position(|&b| b == 0).unwrap_or(ID_LEN);
                return String::from_utf8_lossy(&id[..n]).into_owned();
            }
            core::hint::spin_loop();
        }
        self.abandon(slot);
        String::new()
    }
}

/// How a [`Blk`] call waits for the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// A killed prog stops waiting with `interrupted`; its request finishes
    /// into a buffer nobody reads.
    Interruptible,
    /// Waits for the device whatever happens to the prog.
    Uninterruptible,
}

/// Bumped by the devices' interrupt and when a request frees a slot.
static EVENTS: AtomicU64 = AtomicU64::new(0);
static WAKE: Once<Rendez> = Once::new();
/// The disks' registers, for the interrupt handler, as `Mmio` base
/// addresses (0 for none).
static IRQ_MMIO: [AtomicUsize; MAX_DISKS] = [const { AtomicUsize::new(0) }; MAX_DISKS];

fn device_irq() -> After {
    for m in &IRQ_MMIO {
        let base = m.load(Ordering::Acquire);
        if base != 0 {
            // SAFETY: only `Blk::open_all` stores here, a virtio window.
            unsafe { Mmio::new(base) }.ack();
        }
    }
    EVENTS.fetch_add(1, Ordering::Release);
    if let Some(r) = WAKE.get() {
        r.wakeup();
    }
    After::Return
}

/// Counts of what a disk has done, for tests and `ctl`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub requests: u64,
    /// Requests whose waiter was killed before the device finished.
    pub abandoned: u64,
    pub errors: u64,
}

/// A virtio block device shared by the threads that use it.
pub struct Blk {
    dev: Mutex<VirtioBlk>,
    capacity: u64,
    block_size: usize,
    read_only: bool,
    can_flush: bool,
    xfer: usize,
    serial: String,
    requests: AtomicU64,
    abandoned: AtomicU64,
    errors: AtomicU64,
}

impl Blk {
    /// Brings up every virtio-blk device in the DTB (up to [`MAX_DISKS`]),
    /// asks each for its serial number, and routes their interrupts. Call
    /// once, after [`irq::init`]. A device that fails to come up is
    /// reported in the second list and left out.
    pub fn open_all(fdt: &Fdt<'_>) -> (Vec<Blk>, Vec<&'static str>) {
        let mut disks = Vec::new();
        let mut errors = Vec::new();
        WAKE.call_once(Rendez::new);
        for t in virtio::find_all(fdt, virtio::DEVICE_BLK).take(MAX_DISKS) {
            let mut dev = match VirtioBlk::new(&t) {
                Ok(d) => d,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            let serial = dev.serial_polled();
            IRQ_MMIO[disks.len()].store(t.mmio.base(), Ordering::Release);
            irq::register(t.intid, device_irq);
            disks.push(Blk {
                capacity: dev.capacity(),
                block_size: dev.block_size(),
                read_only: dev.read_only(),
                can_flush: dev.can_flush,
                xfer: dev.xfer(),
                serial,
                dev: Mutex::new(dev),
                requests: AtomicU64::new(0),
                abandoned: AtomicU64::new(0),
                errors: AtomicU64::new(0),
            });
        }
        (disks, errors)
    }

    /// The capacity in 512-byte sectors.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The capacity in bytes.
    pub fn size(&self) -> u64 {
        self.capacity * SECTOR
    }

    /// The logical block size in bytes: every request is whole blocks.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The serial number QEMU was given (`serial=`), or empty.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn stats(&self) -> Stats {
        Stats {
            requests: self.requests.load(Ordering::Relaxed),
            abandoned: self.abandoned.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    /// Requests the device has not finished, abandoned ones included.
    pub fn in_flight(&self) -> usize {
        self.dev.lock().in_flight()
    }

    /// Reads `buf.len()` bytes from byte offset `sector * 512`. Both must
    /// be whole logical blocks.
    pub fn read(&self, sector: u64, buf: &mut [u8], wait: Wait) -> Result<()> {
        self.check(sector, buf.len(), false)?;
        let mut done = 0;
        while done < buf.len() {
            let n = (buf.len() - done).min(self.xfer);
            let s = sector + (done as u64) / SECTOR;
            self.request(Op::Read, s, &mut buf[done..done + n], wait)?;
            done += n;
        }
        Ok(())
    }

    /// Writes `buf` at byte offset `sector * 512`. Both must be whole
    /// logical blocks.
    pub fn write(&self, sector: u64, buf: &[u8], wait: Wait) -> Result<()> {
        self.check(sector, buf.len(), true)?;
        let mut done = 0;
        while done < buf.len() {
            let n = (buf.len() - done).min(self.xfer);
            let s = sector + (done as u64) / SECTOR;
            self.run(Op::Write, s, &buf[done..done + n], &mut [], n, wait)?;
            done += n;
        }
        Ok(())
    }

    /// Waits until everything written so far is on stable storage. A
    /// device without a write cache to flush has nothing to do.
    pub fn flush(&self) -> Result<()> {
        if !self.can_flush {
            return Ok(());
        }
        self.request(Op::Flush, 0, &mut [], Wait::Uninterruptible)
    }

    fn check(&self, sector: u64, len: usize, write: bool) -> Result<()> {
        if write && self.read_only {
            return Err(Cow::Borrowed(error::EPERM));
        }
        let bs = self.block_size as u64;
        let end = (len as u64)
            .checked_add(sector.saturating_mul(SECTOR))
            .ok_or(Cow::Borrowed(error::EBADARG))?;
        if !(sector * SECTOR).is_multiple_of(bs) || !(len as u64).is_multiple_of(bs) {
            return Err(Cow::Borrowed(error::EBADARG));
        }
        if end > self.size() {
            return Err(Cow::Borrowed(error::EIO));
        }
        Ok(())
    }

    fn request(&self, op: Op, sector: u64, out: &mut [u8], wait: Wait) -> Result<()> {
        let len = out.len();
        self.run(op, sector, &[], out, len, wait)
    }

    /// Submits one request and waits for it.
    fn run(
        &self,
        op: Op,
        sector: u64,
        data: &[u8],
        out: &mut [u8],
        len: usize,
        wait: Wait,
    ) -> Result<()> {
        let slot = loop {
            // Read before looking, so an event in between is not missed.
            let seen = EVENTS.load(Ordering::Acquire);
            if let Some(s) = self.dev.lock().submit(op, sector, data, len) {
                break s;
            }
            Self::wait(seen, wait)?;
        };
        self.requests.fetch_add(1, Ordering::Relaxed);
        loop {
            let seen = EVENTS.load(Ordering::Acquire);
            let status = {
                let mut d = self.dev.lock();
                d.reap();
                d.take(slot, out)
            };
            if let Some(status) = status {
                // A slot is free: wake a submit waiting for one.
                Self::poke();
                return match status {
                    S_OK => Ok(()),
                    S_UNSUPP => Err(Cow::Borrowed(error::ENOTIMPL)),
                    _ => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        Err(Cow::Borrowed(error::EIO))
                    }
                };
            }
            if let Err(e) = Self::wait(seen, wait) {
                if self.dev.lock().abandon(slot) {
                    self.abandoned.fetch_add(1, Ordering::Relaxed);
                }
                Self::poke();
                return Err(e);
            }
        }
    }

    fn wake() -> &'static Rendez {
        WAKE.get().expect("Blk::open_all ran")
    }

    /// Sleeps until [`EVENTS`] moves past `seen`, or a safety poll passes.
    fn wait(seen: u64, wait: Wait) -> Result<()> {
        let moved = || EVENTS.load(Ordering::Acquire) != seen;
        match wait {
            Wait::Interruptible => {
                virtio::sleep(Self::wake(), moved, Some(SAFETY_POLL_MS)).map(drop)
            }
            Wait::Uninterruptible => {
                Self::wake().tsleep(moved, SAFETY_POLL_MS * 1_000_000);
                Ok(())
            }
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
