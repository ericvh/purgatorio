//! Secondary CPUs: started with PSCI `CPU_ON` (under TCG and KVM alike),
//! each on the boot CPU's page tables and its own idle thread.
//!
//! For each CPU the DTB lists under `/cpus` other than the boot CPU, the
//! boot CPU makes the CPU's idle thread ([`Scheduler::add_cpu`]), fills in
//! a [`SecondaryBoot`] (the boot CPU's TTBR0/TTBR1, TCR, MAIR and SCTLR,
//! and the stack), cleans it to the point of coherency (the new CPU reads
//! it with the MMU and caches off) and calls `CPU_ON` with the physical
//! address of `purg_secondary_entry` (in [`crate::boot`]) and of the
//! block. The new CPU drops from EL2 to EL1 if it starts there, turns the
//! MMU on, moves to the high half, sets up its exception vectors, its GIC
//! interface and its timer ([`crate::irq::init_cpu`]), says it is up and
//! becomes its idle thread. The boot CPU waits for each in turn.
//!
//! The page tables must not change after this: TLB maintenance in
//! [`crate::mmu`] is local to the CPU that does it (`tlbi vmalle1`), which
//! is right only while one CPU runs. The kernel builds its runtime tables
//! before it starts the secondaries and never changes them after; DMA
//! cache maintenance by address (`dc cvac`, `dc ivac`) already reaches the
//! point of coherency for every CPU.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Once;

use crate::cpu::{self, MAX_CPUS};
use crate::dtb::Fdt;
use crate::mmu;
use crate::sched::Scheduler;
use crate::timer;

/// What a secondary CPU starts from; `purg_secondary_entry` reads the
/// first five words with the MMU off, at their offsets.
#[repr(C, align(64))]
pub struct SecondaryBoot {
    ttbr0: u64,
    ttbr1: u64,
    tcr: u64,
    mair: u64,
    sctlr: u64,
    /// The top of the idle thread's stack, a high-half address.
    stack: u64,
    cpu: u64,
    /// Set by the CPU once it is up.
    started: AtomicU64,
}

const _: () = assert!(core::mem::offset_of!(SecondaryBoot, sctlr) == 32);
const _: () = assert!(core::mem::offset_of!(SecondaryBoot, stack) == 40);

#[allow(clippy::declare_interior_mutable_const)]
const UNUSED: SecondaryBoot = SecondaryBoot {
    ttbr0: 0,
    ttbr1: 0,
    tcr: 0,
    mair: 0,
    sctlr: 0,
    stack: 0,
    cpu: 0,
    started: AtomicU64::new(0),
};

/// One block per CPU; each is written by the boot CPU before its CPU
/// starts and read by that CPU only.
static mut BOOT: [SecondaryBoot; MAX_CPUS] = [UNUSED; MAX_CPUS];

static SCHED: Once<&'static Scheduler> = Once::new();

unsafe extern "C" {
    fn purg_secondary_entry();
}

const PSCI_CPU_ON: u64 = 0xc400_0003;

/// PSCI `CPU_ON` through `hvc` or `smc`. Returns PSCI's status: 0 on
/// success, negative on failure.
fn cpu_on(smc: bool, mpidr: u64, entry: u64, context: u64) -> i64 {
    let r: u64;
    // SAFETY: an SMCCC call to the hypervisor or firmware; it changes no
    // memory of ours, and every register it may clobber is marked so.
    unsafe {
        if smc {
            asm!("smc #0", inlateout("x0") PSCI_CPU_ON => r, in("x1") mpidr, in("x2") entry,
                in("x3") context, clobber_abi("C"), options(nostack));
        } else {
            asm!("hvc #0", inlateout("x0") PSCI_CPU_ON => r, in("x1") mpidr, in("x2") entry,
                in("x3") context, clobber_abi("C"), options(nostack));
        }
    }
    r as i64
}

/// The MPIDRs of the CPUs `/cpus` lists, in order.
pub fn cpus<'a>(fdt: &Fdt<'a>) -> impl Iterator<Item = u64> + 'a {
    fdt.find_path("/cpus")
        .into_iter()
        .flat_map(|c| c.children())
        .filter(|n| n.property_str("device_type") == Some("cpu"))
        .filter_map(|n| n.reg().next().map(|r| r.base))
}

/// Why a CPU did not come up.
#[derive(Clone, Copy, Debug)]
pub enum Error {
    /// `CPU_ON` failed with this PSCI status.
    Psci(u64, i64),
    /// The CPU with this MPIDR did not say it was up within a second.
    Timeout(u64),
}

fn sysreg_state() -> (u64, u64, u64, u64, u64) {
    let (t0, t1, tcr, mair, sctlr): (u64, u64, u64, u64, u64);
    // SAFETY: reads system registers only.
    unsafe {
        asm!(
            "mrs {0}, ttbr0_el1",
            "mrs {1}, ttbr1_el1",
            "mrs {2}, tcr_el1",
            "mrs {3}, mair_el1",
            "mrs {4}, sctlr_el1",
            out(reg) t0, out(reg) t1, out(reg) tcr, out(reg) mair, out(reg) sctlr,
            options(nomem, nostack),
        )
    };
    (t0, t1, tcr, mair, sctlr)
}

/// Cleans `len` bytes at `va` to the point of coherency.
fn clean(va: usize, len: usize) {
    let line = crate::cache::dline();
    let mut a = va & !(line - 1);
    while a < va + len {
        // SAFETY: cache maintenance by VA on mapped memory.
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack)) };
        a += line;
    }
    // SAFETY: a barrier only.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Starts up to `max - 1` secondary CPUs (so `max` in all) that the DTB
/// lists, with PSCI through `smc` or `hvc`, as `sched`'s CPUs 1, 2, ...
/// Call on the boot CPU, after `crate::irq::init` and once the page
/// tables are final. Returns the CPUs online, and the first CPU that did
/// not come up, if one did not.
pub fn start(
    fdt: &Fdt<'_>,
    sched: &'static Scheduler,
    smc: bool,
    max: usize,
) -> (usize, Option<Error>) {
    SCHED.call_once(|| sched);
    let me = cpu::mpidr();
    let (ttbr0, ttbr1, tcr, mair, sctlr) = sysreg_state();
    let entry = mmu::virt_to_phys(purg_secondary_entry as *const () as usize) as u64;
    let mut next = 1;
    for mpidr in cpus(fdt) {
        if next >= max.min(MAX_CPUS) {
            break;
        }
        if mpidr == me {
            continue;
        }
        let cpu = next;
        let stack = sched.add_cpu(cpu);
        // SAFETY: block `cpu` is written here, before its CPU starts, and
        // read by that CPU only afterwards; nothing else touches it.
        let b = unsafe { &mut *(&raw mut BOOT).cast::<SecondaryBoot>().add(cpu) };
        b.ttbr0 = ttbr0;
        b.ttbr1 = ttbr1;
        b.tcr = tcr;
        b.mair = mair;
        b.sctlr = sctlr;
        b.stack = stack as u64;
        b.cpu = cpu as u64;
        b.started.store(0, Ordering::Release);
        let va = b as *const SecondaryBoot as usize;
        clean(va, core::mem::size_of::<SecondaryBoot>());
        let r = cpu_on(smc, mpidr, entry, mmu::virt_to_phys(va) as u64);
        if r != 0 {
            return (sched.online(), Some(Error::Psci(mpidr, r)));
        }
        let deadline = timer::counter() + timer::frequency();
        while b.started.load(Ordering::Acquire) == 0 {
            if timer::counter() > deadline {
                return (sched.online(), Some(Error::Timeout(mpidr)));
            }
            core::hint::spin_loop();
        }
        next += 1;
    }
    (sched.online(), None)
}

/// A secondary CPU's first Rust code, in the high half on its idle
/// thread's stack, with IRQs masked.
#[unsafe(no_mangle)]
extern "C" fn purg_aarch64_secondary_main(b: &'static SecondaryBoot) -> ! {
    let cpu = b.cpu as usize;
    cpu::set_id(cpu);
    cpu::set_thread(0);
    crate::irq::init_cpu();
    let sched = SCHED.get().expect("smp::start ran");
    sched.start_cpu(cpu, || b.started.store(1, Ordering::Release))
}
