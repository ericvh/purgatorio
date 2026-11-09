//! A polling virtio-mmio network device for QEMU's `virt` machine.
//!
//! The transports sit at `0x0a00_0000`, one every `0x200` bytes. Frames are
//! moved with cache maintenance because RAM is mapped write-back and the
//! device reads memory, not the CPU's cache.

use alloc::alloc::{Layout, alloc_zeroed};
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

const MMIO_BASE: usize = 0x0a00_0000;
const MMIO_STRIDE: usize = 0x200;
const SLOTS: usize = 32;

const MAGIC: u32 = 0x7472_6976;
const VERSION: u32 = 2;
const DEV_NET: u32 = 1;

const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;

const F_MAC: u64 = 1 << 5;
const F_VERSION_1: u64 = 1 << 32;

const DESC_WRITE: u16 = 2;

const Q: usize = 8;
const BUF: usize = 2048;
/// `virtio_net_hdr_v1` is 12 bytes once VERSION_1 is negotiated. QEMU writes
/// that size even without `VIRTIO_NET_F_MRG_RXBUF`.
const HDR: usize = 12;

struct Queue {
    mmio: usize,
    index: u32,
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

/// The guest's virtio-net card.
pub struct VirtioNet {
    pub mac: [u8; 6],
    rx: Queue,
    tx: Queue,
}

impl VirtioNet {
    /// Brings up the first virtio-net transport. The caller has already
    /// arranged a fixed MAC on the QEMU command line.
    pub fn open() -> Result<VirtioNet, &'static str> {
        for i in 0..SLOTS {
            let base = MMIO_BASE + i * MMIO_STRIDE;
            let magic = mmio_read(base, 0x000);
            let id = mmio_read(base, 0x008);
            if magic != MAGIC || id != DEV_NET {
                continue;
            }
            if mmio_read(base, 0x004) != VERSION {
                return Err("virtio-net is not version 2");
            }
            return init(base);
        }
        Err("no virtio-net device")
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }
}

fn init(base: usize) -> Result<VirtioNet, &'static str> {
    mmio_write(base, 0x070, 0);
    while mmio_read(base, 0x070) != 0 {}
    mmio_write(base, 0x070, ACKNOWLEDGE);
    mmio_write(base, 0x070, ACKNOWLEDGE | DRIVER);

    let offered = device_features(base);
    let mut want = F_VERSION_1;
    if offered & F_MAC != 0 {
        want |= F_MAC;
    }
    driver_features(base, want & offered);
    mmio_write(base, 0x070, ACKNOWLEDGE | DRIVER | FEATURES_OK);
    if mmio_read(base, 0x070) & FEATURES_OK == 0 {
        return Err("virtio-net rejected features");
    }

    let mac = if want & F_MAC != 0 {
        read_mac(base)
    } else {
        [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
    };
    let rx = Queue::new(base, 0, true)?;
    let tx = Queue::new(base, 1, false)?;
    mmio_write(base, 0x070, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK);
    fence();
    // The kick from queue setup can be ignored before DRIVER_OK.
    rx.notify();
    Ok(VirtioNet { mac, rx, tx })
}

impl Queue {
    fn new(mmio: usize, index: u32, rx: bool) -> Result<Queue, &'static str> {
        mmio_write(mmio, 0x030, index);
        let max = mmio_read(mmio, 0x034) as usize;
        if max < Q {
            return Err("virtio queue is too small");
        }
        let desc = dma(16 * Q);
        let avail = dma(4 + 2 * Q + 2);
        let used = dma(4 + 8 * Q + 2);
        let bufs = dma(BUF * Q);
        mmio_write(mmio, 0x038, Q as u32);
        mmio_write(mmio, 0x080, desc as u32);
        mmio_write(mmio, 0x084, (desc >> 32) as u32);
        mmio_write(mmio, 0x090, avail as u32);
        mmio_write(mmio, 0x094, (avail >> 32) as u32);
        mmio_write(mmio, 0x0a0, used as u32);
        mmio_write(mmio, 0x0a4, (used >> 32) as u32);
        mmio_write(mmio, 0x044, 1);

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
            free: if rx { 0 } else { (1u16 << Q) - 1 },
        };
        for i in 0..Q {
            q.write_desc(
                i as u16,
                q.bufs + i * BUF,
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

    fn write_desc(&self, id: u16, addr: usize, len: u32, flags: u16) {
        let d = self.desc + id as usize * 16;
        unsafe {
            write_volatile(d as *mut u64, addr as u64);
            write_volatile((d + 8) as *mut u32, len);
            write_volatile((d + 12) as *mut u16, flags);
            write_volatile((d + 14) as *mut u16, 0);
        }
    }

    fn enqueue(&mut self, id: u16) {
        let slot = self.avail_idx as usize % Q;
        unsafe { write_volatile((self.avail + 4 + slot * 2) as *mut u16, id) };
        fence();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        unsafe { write_volatile((self.avail + 2) as *mut u16, self.avail_idx) };
        dcache_clean(self.desc, 16 * Q);
        dcache_clean(self.avail, 4 + 2 * Q + 2);
        fence();
    }

    fn notify(&self) {
        mmio_write(self.mmio, 0x050, self.index);
    }

    fn pop_used(&mut self) -> Option<(u16, u32)> {
        dcache_inval(self.used, 4 + 8 * Q + 2);
        let idx = unsafe { read_volatile((self.used + 2) as *const u16) };
        if idx == self.last_used {
            return None;
        }
        let slot = self.last_used as usize % Q;
        let elem = self.used + 4 + slot * 8;
        let id = unsafe { read_volatile(elem as *const u32) } as u16;
        let len = unsafe { read_volatile((elem + 4) as *const u32) };
        self.last_used = self.last_used.wrapping_add(1);
        Some((id, len))
    }

    fn reap(&mut self) {
        while let Some((id, _)) = self.pop_used() {
            if (id as usize) < Q {
                self.free |= 1 << id;
            }
        }
    }

    /// The ethernet frame after the virtio-net header. [`Queue::recycle`]
    /// gives the buffer back; it must run before the next pop.
    fn pop_frame(&mut self) -> Option<&[u8]> {
        loop {
            let (id, len) = self.pop_used()?;
            if (id as usize) >= Q || (len as usize) <= HDR {
                self.give_rx(id);
                self.notify();
                continue;
            }
            let addr = self.bufs + id as usize * BUF;
            dcache_inval(addr, len as usize);
            self.held = Some(id);
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
        self.write_desc(id, self.bufs + id as usize * BUF, BUF as u32, DESC_WRITE);
        self.enqueue(id);
    }

    fn push_frame(&mut self, frame: &[u8]) -> bool {
        self.reap();
        let Some(id) = (0..Q).find(|i| self.free & (1 << i) != 0) else {
            return false;
        };
        if frame.len() + HDR > BUF {
            return false;
        }
        self.free &= !(1 << id);
        let addr = self.bufs + id * BUF;
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
        if self.tx.free == 0 || self.rx.peek().is_none() {
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

impl Queue {
    fn peek(&mut self) -> Option<()> {
        dcache_inval(self.used, 4 + 8 * Q + 2);
        let idx = unsafe { read_volatile((self.used + 2) as *const u16) };
        if idx == self.last_used {
            None
        } else {
            Some(())
        }
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

fn device_features(base: usize) -> u64 {
    mmio_write(base, 0x014, 0);
    let lo = mmio_read(base, 0x010) as u64;
    mmio_write(base, 0x014, 1);
    let hi = mmio_read(base, 0x010) as u64;
    lo | (hi << 32)
}

fn driver_features(base: usize, features: u64) {
    mmio_write(base, 0x024, 0);
    mmio_write(base, 0x020, features as u32);
    mmio_write(base, 0x024, 1);
    mmio_write(base, 0x020, (features >> 32) as u32);
}

fn read_mac(base: usize) -> [u8; 6] {
    let w0 = mmio_read(base, 0x100).to_le_bytes();
    let w1 = mmio_read(base, 0x104).to_le_bytes();
    [w0[0], w0[1], w0[2], w0[3], w1[0], w1[1]]
}

fn mmio_read(base: usize, off: usize) -> u32 {
    unsafe { read_volatile((base + off) as *const u32) }
}

fn mmio_write(base: usize, off: usize, val: u32) {
    unsafe { write_volatile((base + off) as *mut u32, val) }
}

fn dma(size: usize) -> usize {
    let layout = Layout::from_size_align(size, 64).expect("dma layout");
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

fn dcache(addr: usize, len: usize, clean: bool) {
    if len == 0 {
        return;
    }
    let start = addr & !63;
    let end = (addr + len + 63) & !63;
    let mut a = start;
    while a < end {
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
    unsafe { asm!("dsb sy", options(nostack)) }
}
