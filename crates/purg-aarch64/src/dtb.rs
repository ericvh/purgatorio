//! Flattened device tree (DTB) parsing. OWNER: K1 agent A.
//!
//! Required API (host-testable, no allocation):
//! - `Fdt::new(&[u8]) -> Result<Fdt<'_>, Error>` and
//!   `unsafe Fdt::from_ptr(*const u8)` (reads `totalsize` from the header).
//! - `BootInfo::from_fdt(&Fdt) -> Result<BootInfo, Error>` with: RAM
//!   regions (`/memory`), the PL011 base (`arm,pl011`, or `/chosen`
//!   `stdout-path`), the GIC (`arm,cortex-a15-gic` = v2: distributor and
//!   CPU interface; `arm,gic-v3`: distributor and redistributor), the
//!   generic timer's virtual-timer PPI as a GIC INTID, the initrd range
//!   from `/chosen` (`linux,initrd-start`/`-end`) if present, the PL031 RTC
//!   base, and virtio-mmio slots.
