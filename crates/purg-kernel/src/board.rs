//! What the kernel knows about the machine.
//!
//! The boot page tables are built before the MMU is on, from [`board`]:
//! QEMU virt's fixed memory map. Once the MMU is on, `kernel_main` parses
//! the DTB and checks that it agrees; the console, the GIC, the timer and
//! the exit method then come from the DTB.

use purg_aarch64::dtb::{BootInfo, Fdt};
use purg_aarch64::mmu::Region;

/// The machine's memory map (physical addresses).
#[derive(Clone, Copy)]
pub struct Board {
    /// RAM the kernel may use. QEMU virt RAM starts at 1 GiB; `xtask
    /// kernel` runs with `-m 512M`.
    pub ram: Region,
    /// The device window, mapped Device-nGnRnE: the low 1 GiB of physical
    /// space on virt (GIC, PL011, PL031, virtio-mmio, ...).
    pub devices: Region,
    /// The PL011.
    pub uart: usize,
}

/// The QEMU virt memory map the boot page tables are built from. Called
/// before the MMU is on, so it must not touch memory. `kernel_main` checks
/// it against the DTB.
pub const fn board() -> Board {
    Board {
        ram: Region {
            base: 0x4000_0000,
            size: 512 << 20,
        },
        devices: Region {
            base: 0,
            size: 1 << 30,
        },
        uart: 0x0900_0000,
    }
}

/// The flattened device tree magic, big-endian.
pub const FDT_MAGIC: u32 = 0xd00d_feed;

/// How the kernel leaves QEMU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitMethod {
    /// Arm semihosting `SYS_EXIT` (`-semihosting`; TCG only), which carries
    /// the exit code.
    Semihosting,
    /// PSCI `SYSTEM_OFF` through `hvc` or `smc` (`smc = true`). QEMU exits
    /// with 0 whatever the outcome, so pass/fail is in the serial output.
    Psci { smc: bool },
}

fn be32(b: &[u8], off: usize) -> Option<u32> {
    let w = b.get(off..off + 4)?;
    Some(u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
}

/// Chooses the exit method from the DTB: PSCI if `/chosen/bootargs`
/// contains `exit=psci` (`-append exit=psci`), with the conduit from the
/// `/psci` node's `method` (`hvc` unless it says `smc`).
pub fn exit_method(fdt: &Fdt<'_>, info: &BootInfo<'_>) -> ExitMethod {
    let psci = info
        .bootargs
        .is_some_and(|a| a.split_ascii_whitespace().any(|w| w == "exit=psci"));
    if !psci {
        return ExitMethod::Semihosting;
    }
    let smc = fdt
        .find_path("/psci")
        .and_then(|n| n.property_str("method"))
        == Some("smc");
    ExitMethod::Psci { smc }
}

/// The DTB's total size from its header, if `head` starts with the magic.
pub fn dtb_size(head: &[u8]) -> Option<usize> {
    if be32(head, 0)? != FDT_MAGIC {
        return None;
    }
    Some(be32(head, 4)? as usize)
}
