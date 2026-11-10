//! virtio-mmio (version 2) transports, a virtio-net driver and a
//! virtio-9p driver.
//!
//! The transports come from the DTB ([`find`]); each has a register window
//! in the device window and an SPI. [`VirtioNet`] drives a network card
//! with one receive and one transmit queue of [`QUEUE`] buffers each, and
//! implements [`smoltcp::phy::Device`] for [`crate::net`]. [`Virtio9p`]
//! drives a 9P transport (QEMU's `-device virtio-9p-device`): one request
//! queue on which each request is a chain of a T-message the device reads
//! and a buffer it writes the R-message into; [`crate::p9`] makes it
//! blocking.
//!
//! Memory: queues and buffers come from the heap, so the CPU sees them
//! through the linear map and the device gets their physical addresses
//! ([`mmu::virt_to_phys`]). RAM is mapped write-back, and a device need not
//! snoop the CPU's caches (TCG and KVM happen to be coherent, hardware need
//! not be), so the driver cleans what it hands over and invalidates what
//! it takes back.
//!
//! Interrupts: the device raises its SPI when it uses a buffer. The driver
//! never needs it (everything is polled), but a sleeper can wait for it
//! instead of a timer: register a handler that calls [`Mmio::ack`].

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use crate::dtb::{self, Fdt};
use crate::mmu;

const MAGIC: u32 = 0x7472_6976;
const VERSION: u32 = 2;

/// The virtio device ID of a network card.
pub const DEVICE_NET: u32 = 1;
/// The virtio device ID of a 9P transport.
pub const DEVICE_9P: u32 = 9;

// Register offsets.
const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const REG_DEVICE_FEATURES: usize = 0x010;
const REG_DEVICE_FEATURES_SEL: usize = 0x014;
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC: usize = 0x080;
const REG_QUEUE_AVAIL: usize = 0x090;
const REG_QUEUE_USED: usize = 0x0a0;
const REG_CONFIG: usize = 0x100;

const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;

const F_MAC: u64 = 1 << 5;
const F_VERSION_1: u64 = 1 << 32;

/// virtio-9p: the configuration space has a mount tag.
const F_MOUNT_TAG: u64 = 1 << 0;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;

/// Buffers per queue.
pub const QUEUE: usize = 8;
const BUF: usize = 2048;
/// `virtio_net_hdr_v1` is 12 bytes once VERSION_1 is negotiated. QEMU writes
/// that size even without `VIRTIO_NET_F_MRG_RXBUF`.
const HDR: usize = 12;

/// A virtio-mmio register window, mapped as Device memory. `Copy`, so an
/// interrupt handler can keep one to acknowledge the device while a driver
/// owns the rest.
#[derive(Clone, Copy, Debug)]
pub struct Mmio {
    base: usize,
}

impl Mmio {
    /// # Safety
    /// `base` must be the virtual address of a virtio-mmio window, mapped
    /// as Device memory.
    pub const unsafe fn new(base: usize) -> Mmio {
        Mmio { base }
    }

    fn read(&self, off: usize) -> u32 {
        // SAFETY: `new` requires a mapped window; offsets are in it.
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    /// A byte of the configuration space, which is byte-addressed.
    fn read8(&self, off: usize) -> u8 {
        // SAFETY: as in `read`.
        unsafe { read_volatile((self.base + off) as *const u8) }
    }

    fn write(&self, off: usize, val: u32) {
        // SAFETY: as in `read`.
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }

    /// The device ID, 0 for an empty slot, if this is a version 2
    /// transport.
    pub fn device_id(&self) -> Option<u32> {
        (self.read(REG_MAGIC) == MAGIC && self.read(REG_VERSION) == VERSION)
            .then(|| self.read(REG_DEVICE_ID))
    }

    /// Acknowledges the device's pending interrupts, which lowers its
    /// line. Safe to call from an interrupt handler. Returns the
    /// interrupt status (bit 0: a queue was used; bit 1: the configuration
    /// changed).
    pub fn ack(&self) -> u32 {
        let status = self.read(REG_INTERRUPT_STATUS);
        if status != 0 {
            self.write(REG_INTERRUPT_ACK, status);
        }
        status
    }
}

/// A virtio-mmio transport from the DTB.
#[derive(Clone, Copy, Debug)]
pub struct Transport {
    pub mmio: Mmio,
    /// Its interrupt, as a GIC INTID.
    pub intid: u32,
}

/// The first version 2 virtio-mmio transport the DTB lists whose device is
/// `device_id`. The device window must be mapped in the high half.
pub fn find(fdt: &Fdt<'_>, device_id: u32) -> Option<Transport> {
    dtb::virtio_mmio(fdt).find_map(|(reg, intid)| {
        // SAFETY: the DTB names this as a virtio-mmio window, in the device
        // window the high half maps as Device memory.
        let mmio = unsafe { Mmio::new(mmu::phys_to_virt(reg.base as usize)) };
        (mmio.device_id() == Some(device_id)).then_some(Transport { mmio, intid })
    })
}

struct Queue {
    mmio: Mmio,
    index: u32,
    /// Virtual addresses of the descriptor table, the rings and the
    /// buffers.
    desc: usize,
    avail: usize,
    used: usize,
    bufs: usize,
    avail_idx: u16,
    last_used: u16,
    /// Descriptor of the frame [`Queue::pop_frame`] last handed out.
    held: Option<u16>,
    /// Bit `i` set means TX buffer `i` is free. Unused for the receive queue.
    free: u16,
}

/// A virtio network card.
pub struct VirtioNet {
    mac: [u8; 6],
    mmio: Mmio,
    rx: Queue,
    tx: Queue,
}

impl VirtioNet {
    /// Resets and brings up the network card at `t`. Without the MAC
    /// feature the card gets QEMU's default MAC, 52:54:00:12:34:56.
    pub fn new(t: &Transport) -> Result<VirtioNet, &'static str> {
        let m = t.mmio;
        if m.device_id() != Some(DEVICE_NET) {
            return Err("not a virtio-net device");
        }
        m.write(REG_STATUS, 0);
        while m.read(REG_STATUS) != 0 {
            core::hint::spin_loop();
        }
        m.write(REG_STATUS, ACKNOWLEDGE);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER);

        let offered = device_features(&m);
        let mut want = F_VERSION_1;
        if offered & F_MAC != 0 {
            want |= F_MAC;
        }
        driver_features(&m, want & offered);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        if m.read(REG_STATUS) & FEATURES_OK == 0 {
            return Err("virtio-net rejected features");
        }

        let mac = if want & F_MAC != 0 {
            let w0 = m.read(REG_CONFIG).to_le_bytes();
            let w1 = m.read(REG_CONFIG + 4).to_le_bytes();
            [w0[0], w0[1], w0[2], w0[3], w1[0], w1[1]]
        } else {
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
        };
        let rx = Queue::new(m, 0, true)?;
        let tx = Queue::new(m, 1, false)?;
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        fence();
        // The kick from queue setup can be ignored before DRIVER_OK.
        rx.notify();
        Ok(VirtioNet {
            mac,
            mmio: m,
            rx,
            tx,
        })
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// The card's registers, for an interrupt handler's [`Mmio::ack`].
    pub fn mmio(&self) -> Mmio {
        self.mmio
    }
}

impl Queue {
    fn new(mmio: Mmio, index: u32, rx: bool) -> Result<Queue, &'static str> {
        mmio.write(REG_QUEUE_SEL, index);
        let max = mmio.read(REG_QUEUE_NUM_MAX) as usize;
        if max < QUEUE {
            return Err("virtio queue is too small");
        }
        let desc = dma(16 * QUEUE);
        let avail = dma(4 + 2 * QUEUE + 2);
        let used = dma(4 + 8 * QUEUE + 2);
        let bufs = dma(BUF * QUEUE);
        let set = |lo: usize, va: usize| {
            let pa = mmu::virt_to_phys(va) as u64;
            mmio.write(lo, pa as u32);
            mmio.write(lo + 4, (pa >> 32) as u32);
        };
        mmio.write(REG_QUEUE_NUM, QUEUE as u32);
        set(REG_QUEUE_DESC, desc);
        set(REG_QUEUE_AVAIL, avail);
        set(REG_QUEUE_USED, used);
        // The rings were zeroed through the cache; the device reads memory.
        dcache_clean(avail, 4 + 2 * QUEUE + 2);
        dcache_clean(used, 4 + 8 * QUEUE + 2);
        mmio.write(REG_QUEUE_READY, 1);

        let mut q = Queue {
            mmio,
            index,
            desc,
            avail,
            used,
            bufs,
            avail_idx: 0,
            last_used: 0,
            held: None,
            free: if rx { 0 } else { (1u16 << QUEUE) - 1 },
        };
        for i in 0..QUEUE {
            q.write_desc(
                i as u16,
                q.buf(i as u16),
                BUF as u32,
                if rx { DESC_WRITE } else { 0 },
            );
            if rx {
                q.enqueue(i as u16);
            }
        }
        if rx {
            q.notify();
        }
        Ok(q)
    }

    fn buf(&self, id: u16) -> usize {
        self.bufs + id as usize * BUF
    }

    /// Writes descriptor `id` for the buffer at virtual `addr`.
    fn write_desc(&self, id: u16, addr: usize, len: u32, flags: u16) {
        let d = self.desc + id as usize * 16;
        // SAFETY: `d` is descriptor `id` of this queue's table.
        unsafe {
            write_volatile(d as *mut u64, mmu::virt_to_phys(addr) as u64);
            write_volatile((d + 8) as *mut u32, len);
            write_volatile((d + 12) as *mut u16, flags);
            write_volatile((d + 14) as *mut u16, 0);
        }
    }

    fn enqueue(&mut self, id: u16) {
        let slot = self.avail_idx as usize % QUEUE;
        // SAFETY: the slot is in this queue's available ring.
        unsafe { write_volatile((self.avail + 4 + slot * 2) as *mut u16, id) };
        fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: the index field of the available ring.
        unsafe { write_volatile((self.avail + 2) as *mut u16, self.avail_idx) };
        dcache_clean(self.desc, 16 * QUEUE);
        dcache_clean(self.avail, 4 + 2 * QUEUE + 2);
        fence();
    }

    fn notify(&self) {
        self.mmio.write(REG_QUEUE_NOTIFY, self.index);
    }

    fn used_idx(&self) -> u16 {
        dcache_inval(self.used, 4 + 8 * QUEUE + 2);
        // SAFETY: the index field of the used ring.
        unsafe { read_volatile((self.used + 2) as *const u16) }
    }

    fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.used_idx() == self.last_used {
            return None;
        }
        let slot = self.last_used as usize % QUEUE;
        let elem = self.used + 4 + slot * 8;
        // SAFETY: the element is in the used ring, just invalidated.
        let (id, len) = unsafe {
            (
                read_volatile(elem as *const u32) as u16,
                read_volatile((elem + 4) as *const u32),
            )
        };
        self.last_used = self.last_used.wrapping_add(1);
        Some((id, len))
    }

    fn reap(&mut self) {
        while let Some((id, _)) = self.pop_used() {
            if (id as usize) < QUEUE {
                self.free |= 1 << id;
            }
        }
    }

    fn has_used(&self) -> bool {
        self.used_idx() != self.last_used
    }

    /// The ethernet frame after the virtio-net header. [`Queue::recycle`]
    /// gives the buffer back; it must run before the next pop.
    fn pop_frame(&mut self) -> Option<&[u8]> {
        loop {
            let (id, len) = self.pop_used()?;
            if (id as usize) >= QUEUE || (len as usize) <= HDR || len as usize > BUF {
                self.give_rx(id);
                self.notify();
                continue;
            }
            let addr = self.buf(id);
            dcache_inval(addr, len as usize);
            self.held = Some(id);
            // SAFETY: the device wrote `len` bytes of buffer `id`, which
            // stays ours until `recycle`.
            return Some(unsafe {
                core::slice::from_raw_parts((addr + HDR) as *const u8, len as usize - HDR)
            });
        }
    }

    fn recycle(&mut self) {
        if let Some(id) = self.held.take() {
            self.give_rx(id);
            self.notify();
        }
    }

    fn give_rx(&mut self, id: u16) {
        if (id as usize) >= QUEUE {
            return;
        }
        // The CPU must not write back stale lines over what the device
        // writes next.
        dcache_inval(self.buf(id), BUF);
        self.write_desc(id, self.buf(id), BUF as u32, DESC_WRITE);
        self.enqueue(id);
    }

    fn push_frame(&mut self, frame: &[u8]) -> bool {
        self.reap();
        let Some(id) = (0..QUEUE).find(|i| self.free & (1 << i) != 0) else {
            return false;
        };
        if frame.len() + HDR > BUF {
            return false;
        }
        self.free &= !(1 << id);
        let addr = self.buf(id as u16);
        // SAFETY: buffer `id` is free, so the device is not using it.
        unsafe {
            core::ptr::write_bytes(addr as *mut u8, 0, HDR);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), (addr + HDR) as *mut u8, frame.len());
        }
        dcache_clean(addr, HDR + frame.len());
        self.write_desc(id as u16, addr, (HDR + frame.len()) as u32, 0);
        self.enqueue(id as u16);
        self.notify();
        true
    }
}

/// What smoltcp pulls a received frame out through.
pub struct RxToken<'a> {
    q: &'a mut Queue,
}

/// What smoltcp builds a frame to send through.
pub struct TxToken<'a> {
    q: &'a mut Queue,
}

impl smoltcp::phy::Device for VirtioNet {
    type RxToken<'a>
        = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.tx.reap();
        if self.tx.free == 0 || !self.rx.has_used() {
            return None;
        }
        let VirtioNet { rx, tx, .. } = self;
        Some((RxToken { q: rx }, TxToken { q: tx }))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        self.tx.reap();
        if self.tx.free == 0 {
            None
        } else {
            Some(TxToken { q: &mut self.tx })
        }
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

impl smoltcp::phy::RxToken for RxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let Some(frame) = self.q.pop_frame() else {
            return f(&[]);
        };
        let out = f(frame);
        self.q.recycle();
        out
    }
}

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = alloc::vec![0u8; len];
        let out = f(&mut buf);
        let _ = self.q.push_frame(&buf);
        out
    }
}

/// Requests a [`Virtio9p`] keeps in flight at once.
pub const P9_SLOTS: usize = 16;
/// Descriptors on the 9P queue: two per request.
const P9_DESCS: usize = 2 * P9_SLOTS;

/// A virtio 9P transport. Each of [`P9_SLOTS`] slots owns two descriptors
/// (`2i`, the T-message, device-readable, chained to `2i+1`, the R-message
/// buffer, device-writable) and two `msize` buffers, so a request is a copy
/// in, a kick, and later a copy out. The device may answer in any order;
/// [`Virtio9p::pop`] returns each reply with the cookie its request was
/// submitted with.
pub struct Virtio9p {
    mmio: Mmio,
    tag: String,
    /// The largest message, each way.
    msize: usize,
    /// Each buffer's size: `msize` in whole cache lines.
    bsize: usize,
    desc: usize,
    avail: usize,
    used: usize,
    bufs: usize,
    avail_idx: u16,
    last_used: u16,
    /// Bit `i` set means slot `i` is free.
    free: u32,
    cookies: [u64; P9_SLOTS],
}

impl Virtio9p {
    /// Resets and brings up the 9P transport at `t`, with buffers for
    /// messages of up to `msize` bytes.
    pub fn new(t: &Transport, msize: usize) -> Result<Virtio9p, &'static str> {
        let m = t.mmio;
        if m.device_id() != Some(DEVICE_9P) {
            return Err("not a virtio-9p device");
        }
        m.write(REG_STATUS, 0);
        while m.read(REG_STATUS) != 0 {
            core::hint::spin_loop();
        }
        m.write(REG_STATUS, ACKNOWLEDGE);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER);
        let offered = device_features(&m);
        if offered & F_VERSION_1 == 0 {
            return Err("virtio-9p is not a version 1 device");
        }
        let want = (F_VERSION_1 | F_MOUNT_TAG) & offered;
        driver_features(&m, want);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        if m.read(REG_STATUS) & FEATURES_OK == 0 {
            return Err("virtio-9p rejected features");
        }
        // struct virtio_9p_config { le16 tag_len; u8 tag[tag_len]; }
        let tag = if want & F_MOUNT_TAG != 0 {
            let len = u16::from_le_bytes([m.read8(REG_CONFIG), m.read8(REG_CONFIG + 1)]) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| m.read8(REG_CONFIG + 2 + i)).collect();
            String::from_utf8(bytes).map_err(|_| "virtio-9p mount tag is not UTF-8")?
        } else {
            String::new()
        };

        m.write(REG_QUEUE_SEL, 0);
        if (m.read(REG_QUEUE_NUM_MAX) as usize) < P9_DESCS {
            return Err("virtio queue is too small");
        }
        let bsize = msize.next_multiple_of(64);
        let desc = dma(16 * P9_DESCS);
        let avail = dma(4 + 2 * P9_DESCS + 2);
        let used = dma(4 + 8 * P9_DESCS + 2);
        let bufs = dma(2 * bsize * P9_SLOTS);
        let set = |lo: usize, va: usize| {
            let pa = mmu::virt_to_phys(va) as u64;
            m.write(lo, pa as u32);
            m.write(lo + 4, (pa >> 32) as u32);
        };
        m.write(REG_QUEUE_NUM, P9_DESCS as u32);
        set(REG_QUEUE_DESC, desc);
        set(REG_QUEUE_AVAIL, avail);
        set(REG_QUEUE_USED, used);
        // The rings were zeroed through the cache; the device reads memory.
        dcache_clean(avail, 4 + 2 * P9_DESCS + 2);
        dcache_clean(used, 4 + 8 * P9_DESCS + 2);
        m.write(REG_QUEUE_READY, 1);
        m.write(REG_STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
        fence();
        Ok(Virtio9p {
            mmio: m,
            tag,
            msize,
            bsize,
            desc,
            avail,
            used,
            bufs,
            avail_idx: 0,
            last_used: 0,
            free: (1u32 << P9_SLOTS) - 1,
            cookies: [0; P9_SLOTS],
        })
    }

    /// The mount tag (`mount_tag=` in QEMU), empty if the device has none.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn msize(&self) -> usize {
        self.msize
    }

    /// The device's registers, for an interrupt handler's [`Mmio::ack`].
    pub fn mmio(&self) -> Mmio {
        self.mmio
    }

    fn out_buf(&self, slot: usize) -> usize {
        self.bufs + 2 * slot * self.bsize
    }

    fn in_buf(&self, slot: usize) -> usize {
        self.out_buf(slot) + self.bsize
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

    /// Hands the T-message `req` to the device, to be answered with
    /// `cookie`. `Ok(false)` when every slot is in use.
    pub fn submit(&mut self, req: &[u8], cookie: u64) -> Result<bool, &'static str> {
        if req.len() > self.msize {
            return Err("9P message larger than msize");
        }
        let Some(slot) = (0..P9_SLOTS).find(|i| self.free & (1 << i) != 0) else {
            return Ok(false);
        };
        self.free &= !(1 << slot);
        self.cookies[slot] = cookie;
        let (out, inb) = (self.out_buf(slot), self.in_buf(slot));
        // SAFETY: the slot is free, so the device is not using its buffers.
        unsafe { core::ptr::copy_nonoverlapping(req.as_ptr(), out as *mut u8, req.len()) };
        dcache_clean(out, req.len());
        // No dirty line may be written back over what the device writes.
        dcache_inval(inb, self.bsize);
        let head = 2 * slot;
        self.write_desc(head, out, req.len(), DESC_NEXT, (head + 1) as u16);
        self.write_desc(head + 1, inb, self.msize, DESC_WRITE, 0);
        let ring = self.avail_idx as usize % P9_DESCS;
        // SAFETY: the entry is in this queue's available ring.
        unsafe { write_volatile((self.avail + 4 + ring * 2) as *mut u16, head as u16) };
        fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: the index field of the available ring.
        unsafe { write_volatile((self.avail + 2) as *mut u16, self.avail_idx) };
        dcache_clean(self.desc, 16 * P9_DESCS);
        dcache_clean(self.avail, 4 + 2 * P9_DESCS + 2);
        fence();
        self.mmio.write(REG_QUEUE_NOTIFY, 0);
        Ok(true)
    }

    /// The next reply the device has written, with its request's cookie.
    /// Its slot is free again.
    pub fn pop(&mut self) -> Option<(u64, Vec<u8>)> {
        loop {
            dcache_inval(self.used, 4 + 8 * P9_DESCS + 2);
            // SAFETY: the index field of the used ring, just invalidated.
            let idx = unsafe { read_volatile((self.used + 2) as *const u16) };
            if idx == self.last_used {
                return None;
            }
            let elem = self.used + 4 + (self.last_used as usize % P9_DESCS) * 8;
            // SAFETY: the element is in the used ring.
            let (head, len) = unsafe {
                (
                    read_volatile(elem as *const u32) as usize,
                    read_volatile((elem + 4) as *const u32) as usize,
                )
            };
            self.last_used = self.last_used.wrapping_add(1);
            let slot = head / 2;
            if head % 2 != 0 || slot >= P9_SLOTS || self.free & (1 << slot) != 0 {
                continue;
            }
            let inb = self.in_buf(slot);
            let len = len.min(self.msize);
            dcache_inval(inb, len);
            // SAFETY: the device wrote `len` bytes of this slot's reply
            // buffer, and the slot stays ours until it is freed below.
            let mut reply = unsafe { core::slice::from_raw_parts(inb as *const u8, len) }.to_vec();
            // The 9P size is what counts; the device may report more.
            if let Some(&[a, b, c, d]) = reply.get(..4) {
                reply.truncate(u32::from_le_bytes([a, b, c, d]) as usize);
            }
            self.free |= 1 << slot;
            return Some((self.cookies[slot], reply));
        }
    }
}

fn device_features(m: &Mmio) -> u64 {
    m.write(REG_DEVICE_FEATURES_SEL, 0);
    let lo = u64::from(m.read(REG_DEVICE_FEATURES));
    m.write(REG_DEVICE_FEATURES_SEL, 1);
    let hi = u64::from(m.read(REG_DEVICE_FEATURES));
    lo | (hi << 32)
}

fn driver_features(m: &Mmio, features: u64) {
    m.write(REG_DRIVER_FEATURES_SEL, 0);
    m.write(REG_DRIVER_FEATURES, features as u32);
    m.write(REG_DRIVER_FEATURES_SEL, 1);
    m.write(REG_DRIVER_FEATURES, (features >> 32) as u32);
}

/// Zeroed heap memory for the device, in whole 64-byte cache lines so no
/// other allocation shares a line that [`dcache_inval`] discards. Never
/// freed.
fn dma(size: usize) -> usize {
    let layout = Layout::from_size_align(size.next_multiple_of(64), 64).expect("dma layout");
    // SAFETY: a non-zero-size layout.
    let p = unsafe { alloc_zeroed(layout) };
    assert!(!p.is_null(), "out of memory");
    p as usize
}

fn dcache_clean(addr: usize, len: usize) {
    dcache(addr, len, true);
}

fn dcache_inval(addr: usize, len: usize) {
    dcache(addr, len, false);
}

/// Cleans (`dc cvac`) or invalidates (`dc ivac`) the 64-byte lines over
/// `addr..addr + len`. Every DMA region is whole lines ([`dma`]), so
/// invalidating never discards a neighbour's data. (QEMU's cache line is
/// 64 bytes; CTR_EL0 could say otherwise on hardware.)
fn dcache(addr: usize, len: usize, clean: bool) {
    if len == 0 {
        return;
    }
    let start = addr & !63;
    let end = (addr + len + 63) & !63;
    let mut a = start;
    while a < end {
        // SAFETY: cache maintenance by VA on our own mapped memory.
        unsafe {
            if clean {
                asm!("dc cvac, {}", in(reg) a, options(nostack));
            } else {
                asm!("dc ivac, {}", in(reg) a, options(nostack));
            }
        }
        a += 64;
    }
    fence();
}

fn fence() {
    // SAFETY: a full barrier, no other effect.
    unsafe { asm!("dsb sy", options(nostack)) }
}
