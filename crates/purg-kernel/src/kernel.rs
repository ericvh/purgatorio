//! `kernel_main`, the heap, exit, and fatal error reporting.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::asm;
use core::fmt::{self, Write};

use purg_aarch64::board::{board, exit_method, has_bootarg, heap_region};
use purg_aarch64::boot::{self, DtbError, find_dtb};
use purg_aarch64::console::{self, Input, Raw};
use purg_aarch64::dtb::{self, BootInfo, Fdt};
use purg_aarch64::exit;
use purg_aarch64::heap::Heap;
use purg_aarch64::mmu::{self, KERNEL_VBASE, PAGE_SIZE, Region, TableAlloc};

use crate::system::{self, Mode};
use crate::{irq, selftest};

/// The root file system compiled into the image: a ustar archive of
/// `root/bin` and `root/lib` (see build.rs).
static ROOT_TAR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/root.tar"));

#[global_allocator]
static HEAP: Heap = Heap::empty();

/// Prints a line through the console lock (see [`console::print`]).
macro_rules! println {
    ($($arg:tt)*) => {
        purg_aarch64::console::print(format_args!("{}\n", format_args!($($arg)*)))
    };
}
pub(crate) use println;

pub fn fail(args: fmt::Arguments<'_>) -> ! {
    let _ = writeln!(Raw, "kernel: FAILED {args}");
    exit::exit(1)
}

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            fail(format_args!($($arg)*));
        }
    };
}

fn current_pc() -> usize {
    let pc: usize;
    // SAFETY: reads the address of this instruction.
    unsafe { asm!("adr {}, .", out(reg) pc, options(nomem, nostack)) };
    pc
}

fn current_sp() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer.
    unsafe { asm!("mov {}, sp", out(reg) sp, options(nomem, nostack)) };
    sp
}

/// Checks the image's section permissions through the MMU (`AT`) and the
/// tables (`PXN`).
fn check_sections() {
    let img = boot::image();
    let ttbr1 = mmu::AddressSpace::from_root(mmu::ttbr1());
    let va = |r: Region| mmu::phys_to_virt(r.base);
    // SAFETY: the live TTBR1 tables are in RAM, readable in the high half.
    let leaf = |v: usize| unsafe { ttbr1.lookup(KERNEL_VBASE, v) }.map(|(_, d)| d);
    let text = va(img.text);
    check!(
        mmu::probe(text, false) && !mmu::probe(text, true),
        "text is not read-only"
    );
    check!(
        leaf(text).is_some_and(mmu::executable),
        "text is not executable"
    );
    let ro = va(img.rodata);
    check!(
        mmu::probe(ro, false) && !mmu::probe(ro, true),
        "rodata is not read-only"
    );
    check!(
        !leaf(ro).is_some_and(mmu::executable),
        "rodata is executable"
    );
    let data = va(img.data);
    check!(mmu::probe(data, true), "data is not writable");
    check!(
        !leaf(data).is_some_and(mmu::executable),
        "data is executable"
    );
    let guard = va(img.stack) - PAGE_SIZE;
    check!(!mmu::probe(guard, false), "the stack guard page is mapped");
    let sp = current_sp();
    check!(
        sp >= va(img.stack) && sp < va(img.stack) + img.stack.size,
        "sp {sp:#x} is not on the boot stack"
    );
    println!("kernel: sections: text rx, rodata r, data rw, stack guard unmapped");
}

/// Checks that the DTB agrees with the fixed map the boot tables were built
/// from: the RAM they map must be RAM, and the console must be the PL011
/// they reach.
fn check_board(info: &BootInfo<'_>) {
    let b = board();
    let ram = info.ram;
    check!(
        ram.base as usize <= b.ram.base && b.ram.end() <= ram.end() as usize,
        "boot maps RAM {:#x}..{:#x} but the DTB has {:#x}..{:#x}",
        b.ram.base,
        b.ram.end(),
        ram.base,
        ram.end()
    );
    check!(
        info.uart.base as usize == b.uart,
        "the DTB's PL011 is at {:#x}, not {:#x}",
        info.uart.base,
        b.uart
    );
    let in_window = |r: dtb::Region| r.end() as usize <= b.devices.end();
    check!(
        in_window(info.gic.dist) && in_window(info.gic.cpu_or_redist),
        "the GIC is outside the device window"
    );
    println!("kernel: dtb agrees with the boot memory map");
}

/// The heap is the largest stretch of RAM after the image that holds
/// nothing the kernel still needs: the initrd, whose pages the RAM root
/// borrows for good, and the DTB, which `BootInfo` borrows from.
fn init_heap(reserved: &[Region]) -> Region {
    let heap = heap_region(board().ram, boot::image_end(), reserved);
    // SAFETY: RAM after the image, clear of the DTB and initrd, is unused
    // and linear-mapped RW; this runs once, before any allocation.
    unsafe { HEAP.init(heap) };
    heap
}

fn heap_test(heap: Region) {
    let mut v: Vec<u64> = (0..10_000).collect();
    v.retain(|x| x % 3 == 0);
    check!(v.len() == 3334 && v[3333] == 9999, "vec");
    let b = Box::new([7u8; 4096]);
    check!(b.iter().all(|&x| x == 7), "box");
    let mut m = BTreeMap::new();
    for i in 0..1000u32 {
        m.insert(i.wrapping_mul(2_654_435_761), i);
    }
    check!(
        m.len() == 1000 && m.values().sum::<u32>() == 499_500,
        "btreemap"
    );
    // A large block, touched a page at a time, proves the heap reaches far
    // past the image through the linear map.
    let big = 64 << 20;
    let mut l: Vec<u8> = alloc::vec![0; big];
    for i in (0..big).step_by(PAGE_SIZE) {
        l[i] = (i >> 12) as u8;
    }
    check!(
        (0..big).step_by(PAGE_SIZE).all(|i| l[i] == (i >> 12) as u8),
        "large allocation"
    );
    let end = l.as_ptr() as usize + big;
    check!(
        end <= mmu::phys_to_virt(heap.end()),
        "heap block {end:#x} is outside the heap"
    );
    drop(l);
    println!("kernel: heap: vec, box, btreemap, 64 MiB block ok");
}

/// Rebuilds the kernel map in heap pages with the runtime API and switches
/// to it.
fn rebuild_tables() {
    const PAGES: usize = 64;
    let layout = Layout::from_size_align(PAGES * PAGE_SIZE, PAGE_SIZE).expect("layout");
    // SAFETY: a non-zero-size layout. The pages are never freed: the
    // tables in them stay live.
    let pool = unsafe { alloc::alloc::alloc(layout) };
    check!(!pool.is_null(), "no memory for page tables");
    let pool = Region {
        base: mmu::virt_to_phys(pool as usize),
        size: PAGES * PAGE_SIZE,
    };
    // SAFETY: the pool is ours and writable through the linear map.
    let mut alloc = unsafe { TableAlloc::new(pool, KERNEL_VBASE) };
    let map = match boot::kernel_map(&mut alloc) {
        Ok(m) => m,
        Err(e) => fail(format_args!("runtime page tables: {}", e.as_str())),
    };
    // SAFETY: the new tables map the image, stack, heap and devices at the
    // same addresses as the boot tables.
    unsafe { mmu::switch(&map) };
    check!(mmu::ttbr1() == map.ttbr1, "TTBR1 did not switch");
    let probe = Box::new(0x5a5a_u32);
    check!(*probe == 0x5a5a, "heap after the switch");
    check!(alloc.remaining() < PAGES, "no tables were built");
    println!("kernel: page tables rebuilt at runtime");
}

/// The kernel entry point, called by `purg_aarch64::boot` in the high half
/// with the MMU on. `dtb_phys` is x0 at boot: the DTB's physical address,
/// or 0 if QEMU did not pass one.
#[unsafe(no_mangle)]
pub extern "C" fn purg_main(dtb_phys: usize) -> ! {
    let (dtb, from_x0) = match find_dtb(dtb_phys) {
        Ok(d) => d,
        Err(DtbError::NotFound(pa)) => fail(format_args!("no DTB at {pa:#x}")),
        Err(DtbError::OutsideRam(pa)) => fail(format_args!("DTB at {pa:#x} is outside RAM")),
    };
    let fdt = match Fdt::new(dtb) {
        Ok(f) => f,
        Err(e) => fail(format_args!("bad DTB: {e:?}")),
    };
    let info = match BootInfo::from_fdt(&fdt) {
        Ok(i) => i,
        Err(e) => fail(format_args!("DTB: {e:?}")),
    };
    exit::set_method(exit_method(&fdt, &info));

    println!("Purgatorio kernel {} (aarch64)", env!("CARGO_PKG_VERSION"));
    println!("kernel: booted at EL{}", boot::boot_el());
    println!(
        "kernel: dtb {}",
        if from_x0 {
            "from x0"
        } else {
            "at the start of RAM"
        }
    );
    let pc = current_pc();
    let sp = current_sp();
    let main = purg_main as *const () as usize;
    check!(
        pc >= KERNEL_VBASE && sp >= KERNEL_VBASE && main >= KERNEL_VBASE,
        "not in the high half: pc {pc:#x} sp {sp:#x} kernel_main {main:#x}"
    );
    println!("kernel: running in the high half");
    check_board(&info);
    check_sections();
    let initrd = info.initrd.map(|r| Region {
        base: r.base as usize,
        size: r.size as usize,
    });
    let dtb_region = Region {
        base: mmu::virt_to_phys(dtb.as_ptr() as usize),
        size: dtb.len(),
    };
    let none = Region { base: 0, size: 0 };
    let heap = init_heap(&[initrd.unwrap_or(none), dtb_region]);
    heap_test(heap);
    rebuild_tables();
    irq::init(&info);
    irq::wait_ticks(10);
    println!("kernel: timer interrupts at {} Hz", irq::TICK_HZ);
    irq::alternation_test();
    println!("kernel: two threads alternated under timer preemption");
    let cons = console::init(&info, Input::Blocking);
    selftest::lock_test();
    selftest::tsleep_test();
    // The console test needs `xtask kernel` typing `hello` and ^D, so it
    // runs only in test mode; interactively the console belongs to sh.
    let test = has_bootarg(&info, "test");
    if test {
        selftest::console_test(cons);
    }

    // The root is compiled in, as in Inferno's kernels; an initrd named by
    // the DTB replaces it.
    let (archive, from): (&'static [u8], &str) = match initrd {
        // SAFETY: the DTB says the loader put the initrd here; it is in the
        // linear-mapped RAM, outside the heap, and never reused.
        Some(i) => (
            unsafe { core::slice::from_raw_parts(mmu::phys_to_virt(i.base) as *const u8, i.size) },
            "from the initrd",
        ),
        None => (ROOT_TAR, "compiled in"),
    };
    check!(
        archive.iter().any(|&b| b != 0),
        "no root: build the guests (cargo xtask build-root) or boot with -initrd"
    );
    let mode = if test { Mode::Test } else { Mode::Interactive };
    println!("kernel: root {from}");
    let net = system::net::open(&fdt, &info);
    let failed = system::start(archive, cons, info.rtc.map(|r| r.base as usize), net, mode);
    if failed != 0 {
        fail(format_args!("{failed} failed"));
    }
    println!("kernel: ok");
    exit::exit(0)
}

/// Called by the early vectors on an exception before `irq::init`.
#[unsafe(no_mangle)]
extern "C" fn purg_early_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    fail(format_args!(
        "exception {kind} esr={esr:#x} elr={elr:#x} far={far:#x}"
    ))
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    fail(format_args!("panic: {info}"))
}
