//! What the boot code knows about the machine before it can read the DTB,
//! and the choices it makes from the DTB afterwards.
//!
//! The boot page tables are built before the MMU is on, from [`board`]:
//! QEMU virt's fixed memory map. Once the MMU is on, the DTB is parsed and
//! checked against it; the console, the GIC, the timer, the devices and
//! the exit method then come from the DTB.

use crate::dtb::{BootInfo, Fdt};
use crate::mmu::{PAGE_SIZE, Region};

/// The machine's memory map (physical addresses).
#[derive(Clone, Copy, Debug)]
pub struct Board {
    /// RAM the image may use. QEMU virt RAM starts at 1 GiB; `xtask kernel`
    /// and `xtask probe` run with `-m 512M`.
    pub ram: Region,
    /// The device window, mapped Device-nGnRnE: the low 1 GiB of physical
    /// space on virt (GIC, PL011, PL031, virtio-mmio, ...).
    pub devices: Region,
    /// The PL011.
    pub uart: usize,
}

/// The QEMU virt memory map the boot page tables are built from. Called
/// before the MMU is on, so it must not touch memory.
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

/// The DTB's total size from its header, if `head` starts with the magic.
pub fn dtb_size(head: &[u8]) -> Option<usize> {
    let be32 = |off: usize| {
        let w = head.get(off..off + 4)?;
        Some(u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
    };
    if be32(0)? != FDT_MAGIC {
        return None;
    }
    Some(be32(4)? as usize)
}

/// How the image leaves QEMU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitMethod {
    /// Arm semihosting `SYS_EXIT` (`-semihosting`; TCG only), which carries
    /// the exit code.
    Semihosting,
    /// PSCI `SYSTEM_OFF` through `hvc` or `smc` (`smc = true`). QEMU exits
    /// with 0 whatever the outcome, so pass/fail is in the serial output.
    Psci { smc: bool },
}

/// Whether the whitespace-separated `/chosen/bootargs` contain `word`.
pub fn has_bootarg(info: &BootInfo<'_>, word: &str) -> bool {
    info.bootargs
        .is_some_and(|a| a.split_ascii_whitespace().any(|w| w == word))
}

/// Chooses the exit method from the DTB: PSCI if `/chosen/bootargs`
/// contains `exit=psci` (`-append exit=psci`; KVM has no semihosting),
/// with the conduit from the `/psci` node's `method` (`hvc` unless it says
/// `smc`).
pub fn exit_method(fdt: &Fdt<'_>, info: &BootInfo<'_>) -> ExitMethod {
    if !has_bootarg(info, "exit=psci") {
        return ExitMethod::Semihosting;
    }
    let smc = fdt
        .find_path("/psci")
        .and_then(|n| n.property_str("method"))
        == Some("smc");
    ExitMethod::Psci { smc }
}

/// The heap: the largest stretch of `ram` from `start` (the end of the
/// image) on that overlaps none of `reserved` (the DTB, an initrd), with
/// its base page-aligned. At most four reservations are honoured, and
/// nothing is allocated: there is no heap yet.
pub fn heap_region(ram: Region, start: usize, reserved: &[Region]) -> Region {
    let mut best = Region { base: 0, size: 0 };
    let mut base = start;
    let mut cuts = [Region { base: 0, size: 0 }; 4];
    let n = reserved.len().min(cuts.len());
    cuts[..n].copy_from_slice(&reserved[..n]);
    let cuts = &mut cuts[..n];
    cuts.sort_unstable_by_key(|r| r.base);
    for r in cuts.iter().chain(core::iter::once(&Region {
        base: ram.end(),
        size: 0,
    })) {
        if r.end() <= base {
            continue;
        }
        let top = r.base.min(ram.end());
        if top > base && top - base > best.size {
            best = Region {
                base,
                size: top - base,
            };
        }
        base = base.max(r.end().next_multiple_of(PAGE_SIZE));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    static GICV2: &[u8] = include_bytes!("testdata/virt-gicv2.dtb");
    static INITRD: &[u8] = include_bytes!("testdata/virt-initrd.dtb");

    fn r(base: usize, size: usize) -> Region {
        Region { base, size }
    }

    #[test]
    fn dtb_header() {
        assert_eq!(dtb_size(GICV2), Some(GICV2.len()));
        assert_eq!(dtb_size(&GICV2[..3]), None);
        assert_eq!(dtb_size(&[0; 40]), None);
    }

    #[test]
    fn exit_from_bootargs() {
        let fdt = Fdt::new(GICV2).unwrap();
        let info = BootInfo::from_fdt(&fdt).unwrap();
        assert_eq!(exit_method(&fdt, &info), ExitMethod::Semihosting);
        let fdt = Fdt::new(INITRD).unwrap();
        let info = BootInfo::from_fdt(&fdt).unwrap();
        assert!(has_bootarg(&info, "x=y"));
        assert!(!has_bootarg(&info, "x"));
        assert_eq!(exit_method(&fdt, &info), ExitMethod::Semihosting);
    }

    #[test]
    fn heap_avoids_reservations() {
        let ram = r(0x4000_0000, 0x2000_0000);
        // Nothing reserved: everything after the image.
        assert_eq!(
            heap_region(ram, 0x4100_0000, &[]),
            r(0x4100_0000, 0x1f00_0000)
        );
        // A DTB at the start of RAM, before the image, changes nothing.
        assert_eq!(
            heap_region(ram, 0x4100_0000, &[r(0x4000_0000, 0x10_0000)]),
            r(0x4100_0000, 0x1f00_0000)
        );
        // An initrd in the middle: the larger side wins, page-aligned.
        assert_eq!(
            heap_region(ram, 0x4100_0000, &[r(0x4800_0123, 0x100)]),
            r(0x4800_1000, 0x17ff_f000)
        );
        // A DTB near the end of RAM: the stretch before it.
        assert_eq!(
            heap_region(ram, 0x4100_0000, &[r(0x5ff0_0000, 0x10_0000)]),
            r(0x4100_0000, 0x1ef0_0000)
        );
    }
}
