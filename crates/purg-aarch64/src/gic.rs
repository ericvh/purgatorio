//! GICv2 and GICv3 interrupt controllers, for every CPU.
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
//! The boot CPU calls [`Gic::init`], which sets up the distributor and its
//! own interface; every other CPU calls [`Gic::init_cpu`] for its own
//! (GICv2 banks the CPU interface and the SGI and PPI registers per CPU at
//! the same addresses; GICv3 gives each CPU a redistributor). Peripheral
//! interrupts (SPIs) all go to the boot CPU. SGIs, which the kernel uses
//! as IPIs, are sent with [`Gic::send_sgi`] to CPUs named by their index,
//! which [`Gic::init`] and [`Gic::init_cpu`] record.
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
//! On GICv2 the IAR of an SGI carries the CPU that sent it, and the EOI
//! must write the same value back, so [`Gic::ack`] returns the raw IAR in
//! an [`Ack`].

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::cpu::{self, MAX_CPUS};

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
const GICD_SGIR: usize = 0xf00;
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
        /// The redistributor region; each CPU finds its own in it.
        redist: usize,
    },
}

/// An acknowledged interrupt: its INTID, and the raw IAR value its EOI
/// writes back.
#[derive(Clone, Copy, Debug)]
pub struct Ack {
    pub intid: u32,
    raw: u32,
}

/// By CPU index: on GICv2 the CPU interface's target bit (from the banked
/// `GICD_ITARGETSR0`), on GICv3 the redistributor's RD_base. 0 until the
/// CPU has set up its interface.
static CPU_RD: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// By CPU index: MPIDR affinity, for GICv3 SGIs.
static CPU_AFF: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

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
    /// `redist`.
    ///
    /// # Safety
    /// As for [`Gic::v2`]; `redist` must be the start of the redistributor
    /// region from the DTB.
    pub unsafe fn v3(dist: usize, redist: usize) -> Gic {
        Gic::V3 { dist, redist }
    }

    /// The calling CPU's redistributor in the region at `redist`.
    fn find_rd(redist: usize) -> usize {
        let me = affinity();
        let mut rd = redist;
        loop {
            let typer = r64(rd + GICR_TYPER);
            if (typer >> 32) as u32 == me {
                return rd;
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
    }

    /// This CPU's redistributor, once [`Gic::init`] or [`Gic::init_cpu`]
    /// has run on it. IRQs must be masked, so the CPU does not change.
    fn rd(&self) -> usize {
        CPU_RD[cpu::id()].load(Ordering::Acquire)
    }

    /// Number of interrupt lines the distributor implements (SGIs and PPIs
    /// included), capped at 1020.
    fn lines(dist: usize) -> usize {
        let n = ((r32(dist + GICD_TYPER) & 0x1f) as usize + 1) * 32;
        n.min(1020)
    }

    /// Sets up the distributor and the boot CPU's interface. Every
    /// interrupt is disabled, inactive, not pending, level-triggered
    /// (SPIs) and at the same priority. Call once, on the boot CPU, with
    /// IRQs masked, after [`cpu::set_id`].
    pub fn init(&self) {
        match *self {
            Gic::V2 { dist, .. } => {
                w32(dist + GICD_CTLR, 0);
                let lines = Self::lines(dist);
                for i in (32..lines).step_by(32) {
                    let off = i / 8;
                    w32(dist + GICD_ICENABLER + off, !0);
                    w32(dist + GICD_ICPENDR + off, !0);
                    w32(dist + GICD_ICACTIVER + off, !0);
                }
                set_priorities(dist, GICD_IPRIORITYR + 32, lines - 32);
                // SPIs go to the boot CPU.
                let targets = Self::v2_target(dist) * 0x0101_0101;
                for i in (32..lines).step_by(4) {
                    w32(dist + GICD_ITARGETSR + i, targets);
                }
                for i in (32..lines).step_by(16) {
                    w32(dist + GICD_ICFGR + i / 4, 0);
                }
                w32(dist + GICD_CTLR, 1);
            }
            Gic::V3 { dist, .. } => {
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
            }
        }
        self.init_cpu();
    }

    /// This CPU's target bit on a GICv2: `GICD_ITARGETSR0..7` are banked
    /// and read as the reading CPU's own bit. A uniprocessor GIC reads
    /// them as zero and ignores the field.
    fn v2_target(dist: usize) -> u32 {
        match r32(dist + GICD_ITARGETSR) & 0xff {
            0 => 1,
            me => me,
        }
    }

    /// Sets up the calling CPU's interface (and, on GICv3, its
    /// redistributor): its SGIs and PPIs disabled, inactive and at the
    /// default priority, and the priority mask open. [`Gic::init`] does
    /// this for the boot CPU; each other CPU calls it once, with IRQs
    /// masked, after [`cpu::set_id`].
    pub fn init_cpu(&self) {
        let me = cpu::id();
        CPU_AFF[me].store(mpidr() & 0xff_00ff_ffff, Ordering::Relaxed);
        match *self {
            Gic::V2 { dist, cpu } => {
                // The SGI and PPI registers are banked per CPU.
                w32(dist + GICD_ICENABLER, !0);
                w32(dist + GICD_ICPENDR, !0);
                w32(dist + GICD_ICACTIVER, !0);
                set_priorities(dist, GICD_IPRIORITYR, 32);
                w32(cpu + GICC_PMR, PMR as u32);
                w32(cpu + GICC_BPR, 0);
                w32(cpu + GICC_CTLR, 1);
                CPU_RD[me].store(Self::v2_target(dist) as usize, Ordering::Release);
            }
            Gic::V3 { redist, .. } => {
                let rd = Self::find_rd(redist);
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
                CPU_RD[me].store(rd, Ordering::Release);
            }
        }
    }

    /// Sends SGI `intid` (0..16) to every CPU, by index, in `cpus` that
    /// has set up its interface.
    pub fn send_sgi(&self, intid: u32, cpus: u64) {
        match *self {
            Gic::V2 { dist, .. } => {
                let mut list = 0u32;
                for (c, bit) in CPU_RD.iter().enumerate() {
                    if cpus & (1 << c) != 0 {
                        list |= bit.load(Ordering::Acquire) as u32;
                    }
                }
                if list != 0 {
                    // SAFETY: a barrier, so this CPU's writes are visible
                    // before the targets are interrupted.
                    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
                    w32(dist + GICD_SGIR, (list & 0xff) << 16 | (intid & 0xf));
                }
            }
            Gic::V3 { .. } => {
                for (c, aff) in CPU_AFF.iter().enumerate() {
                    if cpus & (1 << c) == 0 || CPU_RD[c].load(Ordering::Acquire) == 0 {
                        continue;
                    }
                    let a = aff.load(Ordering::Relaxed);
                    let aff0 = a & 0xff;
                    // ICC_SGI1R_EL1: TargetList (a bit of Aff0 within its
                    // group of 16; RS is the group), Aff1, INTID, Aff2,
                    // Aff3.
                    let v = (1 << (aff0 & 0xf))
                        | ((a >> 8) & 0xff) << 16
                        | u64::from(intid & 0xf) << 24
                        | ((a >> 16) & 0xff) << 32
                        | (aff0 >> 4) << 44
                        | ((a >> 32) & 0xff) << 48;
                    // SAFETY: writes ICC_SGI1R_EL1 (S3_0_C12_C11_5); the
                    // dsb makes this CPU's writes visible first.
                    unsafe {
                        asm!("dsb ishst", "msr S3_0_C12_C11_5, {}", "isb", in(reg) v, options(nostack))
                    };
                }
            }
        }
    }

    /// The register block and bit for `intid`'s enable bits: the
    /// redistributor's SGI frame for SGIs and PPIs on GICv3, the
    /// distributor otherwise.
    fn bank(&self, intid: u32) -> (usize, usize, u32) {
        let base = match *self {
            Gic::V3 { .. } if intid < 32 => self.rd() + GICR_SGI,
            Gic::V3 { dist, .. } | Gic::V2 { dist, .. } => dist,
        };
        (base, (intid / 32) as usize * 4, 1 << (intid % 32))
    }

    /// Unmasks `intid` at the GIC: for an SGI or PPI, on the calling CPU
    /// only.
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
            Gic::V3 { .. } if intid < 32 => wait_clear(self.rd() + GICR_CTLR, GICR_CTLR_RWP),
            Gic::V3 { dist, .. } => wait_clear(dist + GICD_CTLR, GICD_CTLR_RWP),
            Gic::V2 { .. } => {}
        }
    }

    /// Acknowledges the highest-priority pending interrupt, or returns
    /// `None` if there is none (a spurious INTID, 1020..=1023). Every
    /// `Some` must be followed by [`Gic::eoi`] with it.
    pub fn ack(&self) -> Option<Ack> {
        let (intid, raw) = match *self {
            Gic::V2 { cpu, .. } => {
                let raw = r32(cpu + GICC_IAR) & 0x1fff;
                (raw & 0x3ff, raw)
            }
            Gic::V3 { .. } => {
                let iar: u64;
                // SAFETY: reads ICC_IAR1_EL1, acknowledging the interrupt.
                // The dsb orders the acknowledge before the handler's
                // device accesses.
                unsafe { asm!("mrs {}, S3_0_C12_C12_0", "dsb sy", out(reg) iar, options(nostack)) };
                let id = (iar & 0xff_ffff) as u32;
                (id, id)
            }
        };
        (!(SPURIOUS..=1023).contains(&intid)).then_some(Ack { intid, raw })
    }

    /// Signals the end of handling `a` (priority drop and deactivate).
    pub fn eoi(&self, a: Ack) {
        match *self {
            Gic::V2 { cpu, .. } => w32(cpu + GICC_EOIR, a.raw),
            // SAFETY: writes ICC_EOIR1_EL1.
            Gic::V3 { .. } => unsafe {
                asm!("msr S3_0_C12_C12_1, {}", "isb", in(reg) u64::from(a.raw), options(nostack))
            },
        }
    }
}
