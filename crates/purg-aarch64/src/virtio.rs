//! virtio-mmio (version 2) transports and a virtio-net driver.
//!
//! The transports come from the DTB ([`find`]); each has a register window
//! in the device window and an SPI. [`VirtioNet`] drives a network card
//! with one receive and one transmit queue of [`QUEUE`] buffers each, and
//! implements [`smoltcp::phy::Device`] for [`crate::net`].
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
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use crate::dtb::{self, Fdt};
use crate::mmu;

const MAGIC: u32 = 0x7472_6976;
const VERSION: u32 = 2;

/// The virtio device ID of a network card.
pub const DEVICE_NET: u32 = 1;

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
