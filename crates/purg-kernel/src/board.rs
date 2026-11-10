//! What the kernel knows about the machine. Until `purg_aarch64::dtb`
//! lands this is QEMU virt's fixed memory map and a crude scan of the DTB
//! blob; every such shortcut is marked `K1: from the DTB`.

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

/// The QEMU virt memory map. Called before the MMU is on, so it must not
/// touch memory.
// K1: from the DTB once dtb.rs lands
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

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Chooses the exit method from the DTB: PSCI if `/chosen/bootargs`
/// contains `exit=psci` (`-append exit=psci`), with the conduit from the
/// `/psci` node's `method` (`hvc` unless the blob has an `smc` method).
/// `dtb` is the whole blob (`totalsize` bytes).
// K1: from the DTB once dtb.rs lands. This scans the raw blob for the
// strings instead of parsing /chosen/bootargs and /psci/method.
pub fn exit_method(dtb: &[u8]) -> ExitMethod {
    if !contains(dtb, b"exit=psci") {
        return ExitMethod::Semihosting;
    }
    // Property values are NUL-terminated and 4-byte aligned in the
    // structure block; "smc\0" or "hvc\0" there is the PSCI method.
    let start = be32(dtb, 8).unwrap_or(0) as usize; // off_dt_struct
    let size = be32(dtb, 36).unwrap_or(0) as usize; // size_dt_struct
    let st = dtb.get(start..start.saturating_add(size)).unwrap_or(&[]);
    let smc = st.as_chunks::<4>().0.iter().any(|w| w == b"smc\0");
    ExitMethod::Psci { smc }
}

/// The DTB's total size from its header, if `head` starts with the magic.
pub fn dtb_size(head: &[u8]) -> Option<usize> {
    if be32(head, 0)? != FDT_MAGIC {
        return None;
    }
    Some(be32(head, 4)? as usize)
}
