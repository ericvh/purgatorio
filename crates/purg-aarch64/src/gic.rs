//! GICv2 and GICv3 interrupt controllers, for the boot CPU.
//!
//! Base addresses come from the DTB and must be mapped as Device memory.
//!
//! - GICv2: distributor and CPU interface are both MMIO. Interrupts stay in
//!   group 0 (the reset value), which a GIC without security extensions,
//!   such as QEMU `virt` with `secure=off`, signals as IRQ.
//! - GICv3: distributor and redistributor are MMIO; the CPU interface is the
//!   `ICC_*` system registers. All interrupts go to group 1 non-secure and
//!   are routed to the boot CPU, as Linux does.
//!
//! **Boot requirement for GICv3:** if the kernel starts at EL2, the boot
//! code must set `ICC_SRE_EL2.SRE` and `ICC_SRE_EL2.Enable` (bits 0 and 3)
//! before dropping to EL1, or `ICC_SRE_EL1.SRE` stays 0 and the system
//! register interface traps or reads as zero. [`Gic::init`] panics if it
//! cannot set `ICC_SRE_EL1.SRE`.
//!
//! Every interrupt starts disabled with the same priority. A driver calls
//! [`Gic::enable`] for its INTID; the IRQ handler calls [`Gic::ack`], does
//! its work and calls [`Gic::eoi`] (EOImode 0: priority drop and deactivate
//! in one step).
//!
//! On GICv2, [`Gic::ack`] drops the source-CPU field of an SGI, so an SGI is
//! EOId as if CPU 0 sent it. That is enough for one CPU; SMP (K4) needs the
//! raw IAR.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

/// Priority given to every interrupt. Lower is more urgent; 0xa0 is
/// visible from both security states.
const DEFAULT_PRIORITY: u8 = 0xa0;
/// Priority mask: accept anything more urgent than 0xf0.
const PMR: u64 = 0xf0;

// Distributor (both versions).
const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_IGROUPR: usize = 0x080;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_ICPENDR: usize = 0x280;
const GICD_ICACTIVER: usize = 0x380;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xc00;
const GICD_IROUTER: usize = 0x6000;
const GICD_CTLR_RWP: u32 = 1 << 31;
/// GICv3, non-secure view: ARE_NS, EnableGrp1A, EnableGrp1. With a single
/// security state the same bits are ARE, EnableGrp1, EnableGrp0.
const GICD_CTLR_V3: u32 = (1 << 4) | (1 << 1) | 1;

// GICv2 CPU interface.
const GICC_CTLR: usize = 0x00;
const GICC_PMR: usize = 0x04;
const GICC_BPR: usize = 0x08;
const GICC_IAR: usize = 0x0c;
const GICC_EOIR: usize = 0x10;

// GICv3 redistributor: RD_base frame, then the SGI_base frame 64 KiB on.
const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008;
const GICR_WAKER: usize = 0x0014;
const GICR_SGI: usize = 0x1_0000;
const GICR_CTLR_RWP: u32 = 1 << 3;
const GICR_TYPER_VLPIS: u64 = 1 << 1;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_WAKER_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;

/// INTIDs 1020..=1023 are special: nothing to handle.
const SPURIOUS: u32 = 1020;

/// A GIC at mapped MMIO addresses.
#[derive(Clone, Copy, Debug)]
pub enum Gic {
    V2 {
        dist: usize,
        cpu: usize,
    },
    V3 {
        dist: usize,
        /// This CPU's redistributor (its RD_base frame).
        rd: usize,
    },
}

fn r32(addr: usize) -> u32 {
    // SAFETY: callers pass addresses inside the GIC frames that the `Gic`
    // constructors' contracts say are mapped.
    unsafe { read_volatile(addr as *const u32) }
}

fn w32(addr: usize, v: u32) {
    // SAFETY: as in `r32`.
    unsafe { write_volatile(addr as *mut u32, v) }
}

fn r64(addr: usize) -> u64 {
    // SAFETY: as in `r32`.
    unsafe { read_volatile(addr as *const u64) }
}

fn w64(addr: usize, v: u64) {
    // SAFETY: as in `r32`.
    unsafe { write_volatile(addr as *mut u64, v) }
}

fn mpidr() -> u64 {
    let v: u64;
    // SAFETY: reading MPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)) };
    v
}

/// This CPU's affinity, Aff3.Aff2.Aff1.Aff0, packed as in GICR_TYPER[63:32].
fn affinity() -> u32 {
    let m = mpidr();
    (((m >> 32) & 0xff) << 24 | (m & 0xff_ffff)) as u32
}

fn wait_clear(addr: usize, bit: u32) {
    while r32(addr) & bit != 0 {
        core::hint::spin_loop();
    }
}

/// Sets `count` priority bytes, starting at register offset `first`, to
/// [`DEFAULT_PRIORITY`].
fn set_priorities(base: usize, first: usize, count: usize) {
    let word = u32::from_ne_bytes([DEFAULT_PRIORITY; 4]);
    for i in (first..first + count).step_by(4) {
        w32(base + i, word);
    }
}

impl Gic {
    /// A GICv2 with its distributor at `dist` and CPU interface at `cpu`.
    ///
    /// # Safety
    /// Both must be the GIC's frames, mapped as Device memory, and only one
    /// `Gic` may drive them.
    pub unsafe fn v2(dist: usize, cpu: usize) -> Gic {
        Gic::V2 { dist, cpu }
    }

    /// A GICv3 with its distributor at `dist` and redistributor region at
    /// `redist`. Finds this CPU's redistributor in the region.
    ///
    /// # Safety
    /// As for [`Gic::v2`]; `redist` must be the start of the redistributor
    /// region from the DTB.
    pub unsafe fn v3(dist: usize, redist: usize) -> Gic {
        let me = affinity();
        let mut rd = redist;
        loop {
            let typer = r64(rd + GICR_TYPER);
            if (typer >> 32) as u32 == me {
                break;
            }
            assert!(
                typer & GICR_TYPER_LAST == 0,
                "gic: no redistributor for this CPU"
            );
            rd += if typer & GICR_TYPER_VLPIS != 0 {
                0x4_0000
            } else {
                0x2_0000
            };
        }
        Gic::V3 { dist, rd }
    }

    /// Number of interrupt lines the distributor implements (SGIs and PPIs
    /// included), capped at 1020.
    fn lines(dist: usize) -> usize {
        let n = ((r32(dist + GICD_TYPER) & 0x1f) as usize + 1) * 32;
        n.min(1020)
    }

    /// Sets up the distributor and this CPU's interface. Every interrupt is
    /// disabled, inactive, not pending, level-triggered (SPIs) and at the
    /// same priority. Call once, with IRQs masked.
    pub fn init(&self) {
        match *self {
            Gic::V2 { dist, cpu } => {
                w32(dist + GICD_CTLR, 0);
                let lines = Self::lines(dist);
                for i in (0..lines).step_by(32) {
                    let off = i / 8;
                    w32(dist + GICD_ICENABLER + off, !0);
                    w32(dist + GICD_ICPENDR + off, !0);
                    w32(dist + GICD_ICACTIVER + off, !0);
                }
                set_priorities(dist, GICD_IPRIORITYR, lines);
                // ITARGETSR0..7 read as this CPU's own mask; a uniprocessor
                // GIC reads them as zero and ignores the field.
                let mut me = r32(dist + GICD_ITARGETSR) & 0xff;
                if me == 0 {
                    me = 1;
                }
                let targets = me * 0x0101_0101;
                for i in (32..lines).step_by(4) {
                    w32(dist + GICD_ITARGETSR + i, targets);
                }
                for i in (32..lines).step_by(16) {
                    w32(dist + GICD_ICFGR + i / 4, 0);
                }
                w32(dist + GICD_CTLR, 1);

                w32(cpu + GICC_PMR, PMR as u32);
                w32(cpu + GICC_BPR, 0);
                w32(cpu + GICC_CTLR, 1);
            }
            Gic::V3 { dist, rd } => {
                w32(dist + GICD_CTLR, 0);
                wait_clear(dist + GICD_CTLR, GICD_CTLR_RWP);
                let lines = Self::lines(dist);
                for i in (32..lines).step_by(32) {
                    let off = i / 8;
                    w32(dist + GICD_IGROUPR + off, !0);
                    w32(dist + GICD_ICENABLER + off, !0);
                    w32(dist + GICD_ICPENDR + off, !0);
                    w32(dist + GICD_ICACTIVER + off, !0);
                }
                set_priorities(dist, GICD_IPRIORITYR + 32, lines - 32);
                for i in (32..lines).step_by(16) {
                    w32(dist + GICD_ICFGR + i / 4, 0);
                }
                wait_clear(dist + GICD_CTLR, GICD_CTLR_RWP);
                w32(dist + GICD_CTLR, GICD_CTLR_V3);
                wait_clear(dist + GICD_CTLR, GICD_CTLR_RWP);
                let m = mpidr();
                let route = m & 0xff_ffff | ((m >> 32) & 0xff) << 32;
                for i in 32..lines {
                    w64(dist + GICD_IROUTER + i * 8, route);
                }

                // Wake this CPU's redistributor.
                let waker = r32(rd + GICR_WAKER);
                w32(rd + GICR_WAKER, waker & !GICR_WAKER_SLEEP);
                wait_clear(rd + GICR_WAKER, GICR_WAKER_CHILDREN_ASLEEP);

                // SGIs and PPIs live in the redistributor.
                let sgi = rd + GICR_SGI;
                w32(sgi + GICD_IGROUPR, !0);
                w32(sgi + GICD_ICENABLER, !0);
                w32(sgi + GICD_ICPENDR, !0);
                w32(sgi + GICD_ICACTIVER, !0);
                set_priorities(sgi, GICD_IPRIORITYR, 32);
                wait_clear(rd + GICR_CTLR, GICR_CTLR_RWP);

                // CPU interface: system registers, group 1, EOImode 0.
                let sre: u64;
                // SAFETY: ICC_SRE_EL1 (S3_0_C12_C12_5): sets SRE, keeps
                // the other bits.
                unsafe {
                    asm!(
                        "mrs {t}, S3_0_C12_C12_5",
                        "orr {t}, {t}, #1",
                        "msr S3_0_C12_C12_5, {t}",
                        "isb",
                        "mrs {t}, S3_0_C12_C12_5",
                        t = out(reg) sre,
                        options(nostack),
                    )
                };
                assert!(
                    sre & 1 != 0,
                    "gic: ICC_SRE_EL1.SRE is stuck at 0 (set ICC_SRE_EL2 at EL2)"
                );
                // SAFETY: ICC_PMR_EL1, ICC_BPR1_EL1, ICC_CTLR_EL1 and
                // ICC_IGRPEN1_EL1, now that the system register interface
                // is on.
                unsafe {
                    asm!(
                        "msr S3_0_C4_C6_0, {pmr}",
                        "msr S3_0_C12_C12_3, xzr",
                        "msr S3_0_C12_C12_4, xzr",
                        "msr S3_0_C12_C12_7, {one}",
                        "isb",
                        pmr = in(reg) PMR,
                        one = in(reg) 1u64,
                        options(nostack),
                    )
                };
            }
        }
    }

    /// The register block and bit for `intid`'s enable bits: the
    /// redistributor's SGI frame for SGIs and PPIs on GICv3, the
    /// distributor otherwise.
    fn bank(&self, intid: u32) -> (usize, usize, u32) {
        let base = match *self {
            Gic::V3 { rd, .. } if intid < 32 => rd + GICR_SGI,
            Gic::V3 { dist, .. } | Gic::V2 { dist, .. } => dist,
        };
        (base, (intid / 32) as usize * 4, 1 << (intid % 32))
    }

    /// Unmasks `intid` at the GIC.
    pub fn enable(&self, intid: u32) {
        let (base, off, bit) = self.bank(intid);
        w32(base + GICD_ISENABLER + off, bit);
    }

    /// Masks `intid` at the GIC. On GICv3 this waits until the change has
    /// taken effect.
    pub fn disable(&self, intid: u32) {
        let (base, off, bit) = self.bank(intid);
        w32(base + GICD_ICENABLER + off, bit);
        match *self {
            Gic::V3 { rd, .. } if intid < 32 => wait_clear(rd + GICR_CTLR, GICR_CTLR_RWP),
            Gic::V3 { dist, .. } => wait_clear(dist + GICD_CTLR, GICD_CTLR_RWP),
            Gic::V2 { .. } => {}
        }
    }

    /// Acknowledges the highest-priority pending interrupt and returns its
    /// INTID, or `None` if there is none (a spurious INTID, 1020..=1023).
    /// Every `Some` must be followed by [`Gic::eoi`] with the same INTID.
    pub fn ack(&self) -> Option<u32> {
        let intid = match *self {
            Gic::V2 { cpu, .. } => r32(cpu + GICC_IAR) & 0x3ff,
            Gic::V3 { .. } => {
                let iar: u64;
                // SAFETY: reads ICC_IAR1_EL1, acknowledging the interrupt.
                // The dsb orders the acknowledge before the handler's
                // device accesses.
                unsafe { asm!("mrs {}, S3_0_C12_C12_0", "dsb sy", out(reg) iar, options(nostack)) };
                (iar & 0xff_ffff) as u32
            }
        };
        (!(SPURIOUS..=1023).contains(&intid)).then_some(intid)
    }

    /// Signals the end of handling `intid` (priority drop and deactivate).
    pub fn eoi(&self, intid: u32) {
        match *self {
            Gic::V2 { cpu, .. } => w32(cpu + GICC_EOIR, intid),
            // SAFETY: writes ICC_EOIR1_EL1.
            Gic::V3 { .. } => unsafe {
                asm!("msr S3_0_C12_C12_1, {}", "isb", in(reg) u64::from(intid), options(nostack))
            },
        }
    }
}
