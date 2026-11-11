//! `kernel_main`, the heap, exit, and fatal error reporting.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::asm;
use core::fmt::{self, Write};

use purg_aarch64::board::{FREE_REGIONS, board, bootarg, exit_method, free_regions, has_bootarg};
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
pub(crate) static HEAP: Heap = Heap::empty();

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
/// from: RAM must start where they map it and hold the image, and the
/// console must be the PL011 they reach. Returns the machine's RAM. It may
/// be smaller or larger than the boot map's window: the heap and the
/// runtime tables are sized from it.
fn check_board(info: &BootInfo<'_>) -> Region {
    let b = board();
    let ram = Region {
        base: info.ram.base as usize,
        size: info.ram.size as usize,
    };
    check!(
        ram.base == b.ram.base,
        "boot maps RAM at {:#x} but the DTB has it at {:#x}",
        b.ram.base,
        ram.base
    );
    check!(
        boot::image_end() <= ram.end(),
        "the image ends at {:#x}, past the end of RAM at {:#x}",
        boot::image_end(),
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
    ram
}

/// The part of `ram` the boot tables map.
fn boot_mapped(ram: Region) -> Region {
    Region {
        base: ram.base,
        size: ram.size.min(board().ram.size),
    }
}

/// The heap is the RAM after the image that holds
/// nothing the kernel still needs: the initrd, whose pages the RAM root
/// borrows for good, and the DTB, which `BootInfo` borrows from.
///
/// It starts inside the boot map; [`grow_heap`] adds RAM beyond it once the
/// runtime tables map that.
fn init_heap(ram: Region, reserved: &[Region]) -> [Region; FREE_REGIONS] {
    let heap = free_regions(boot_mapped(ram), boot::image_end(), reserved);
    let size: usize = heap.iter().map(|r| r.size).sum();
    check!(
        size >= MIN_HEAP,
        "no room for a heap: {} KiB of RAM is free after the image",
        size >> 10
    );
    // SAFETY: RAM after the image, clear of the DTB and initrd, is unused
    // and linear-mapped RW; this runs once, before any allocation.
    unsafe { HEAP.init(&heap) };
    heap
}

fn heap_test(heap: &[Region]) {
    let largest = heap.iter().map(|r| r.size).max().unwrap_or(0);
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
    // past the image through the linear map: 64 MiB, or a quarter of a
    // small heap.
    let big = (64 << 20).min(largest / 4).max(1 << 20) & !((1 << 20) - 1);
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
        heap.iter()
            .any(|r| end > mmu::phys_to_virt(r.base) && end <= mmu::phys_to_virt(r.end())),
        "heap block {end:#x} is outside the heap"
    );
    drop(l);
    println!(
        "kernel: heap: vec, box, btreemap, {} MiB block ok",
        big >> 20
    );
}

/// The least heap the kernel will start with.
const MIN_HEAP: usize = 1 << 20;

/// Builds the kernel map over `ram` in `pages` fresh heap pages. Returns
/// the map, the pool's virtual address and the pages left over.
fn build_tables(ram: Region, pages: usize) -> (mmu::KernelMap, *mut u8, usize) {
    let layout = Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE).expect("layout");
    // SAFETY: a non-zero-size layout.
    let va = unsafe { alloc::alloc::alloc(layout) };
    check!(!va.is_null(), "no memory for page tables");
    let pool = Region {
        base: mmu::virt_to_phys(va as usize),
        size: pages * PAGE_SIZE,
    };
    // SAFETY: the pool is ours and writable through the linear map.
    let mut alloc = unsafe { TableAlloc::new(pool, KERNEL_VBASE) };
    match boot::kernel_map_for(&mut alloc, ram) {
        Ok(m) => (m, va, alloc.remaining()),
        Err(e) => fail(format_args!("runtime page tables: {}", e.as_str())),
    }
}

/// Rebuilds the kernel map in heap pages with the runtime API, over the
/// RAM the DTB reports, and switches to it. Returns the pages they take.
fn rebuild_tables(ram: Region) -> usize {
    // A first build in a generous pool counts the pages the tables need;
    // the tables that stay are then built in exactly that many.
    const PAGES: usize = 64;
    let (_, va, left) = build_tables(ram, PAGES);
    check!(left < PAGES, "no tables were built");
    let layout = Layout::from_size_align(PAGES * PAGE_SIZE, PAGE_SIZE).expect("layout");
    // SAFETY: allocated in `build_tables` with this layout; the tables in
    // it were never loaded.
    unsafe { alloc::alloc::dealloc(va, layout) };
    // The pages are never freed: the tables in them stay live.
    let pages = PAGES - left;
    let (map, _, left) = build_tables(ram, pages);
    check!(left == 0, "the second table build differed from the first");
    // SAFETY: the new tables map the image, stack, heap and devices at the
    // same addresses as the boot tables.
    unsafe { mmu::switch(&map) };
    check!(mmu::ttbr1() == map.ttbr1, "TTBR1 did not switch");
    let probe = Box::new(0x5a5a_u32);
    check!(*probe == 0x5a5a, "heap after the switch");
    println!("kernel: page tables rebuilt at runtime");
    pages
}

/// Gives the heap the RAM past the boot map's window, which the runtime
/// tables now map, when the heap ends at the window's end and nothing is
/// reserved beyond it. Returns the bytes added.
fn grow_heap(ram: Region, reserved: &[Region]) -> usize {
    let mapped = boot_mapped(ram).end();
    if ram.end() <= mapped {
        return 0;
    }
    if reserved.iter().any(|r| r.size > 0 && r.end() > mapped) {
        return 0;
    }
    let by = ram.end() - mapped;
    // SAFETY: RAM from the DTB, directly after a heap region (or `extend`
    // does nothing), mapped by the runtime tables and clear of every
    // reservation.
    if unsafe { HEAP.extend(mapped, by) } {
        by
    } else {
        0
    }
}

/// The byte the boot stack is painted with in `memstat` mode.
const PAINT: u8 = 0xa5;

/// Paints the unused part of the boot stack, below this frame.
fn paint_boot_stack() {
    let lo = mmu::phys_to_virt(boot::image().stack.base);
    let hi = current_sp() - 256;
    // SAFETY: the boot stack below the live frames is unused.
    unsafe { core::ptr::write_bytes(lo as *mut u8, PAINT, hi - lo) };
}

/// Bytes of the boot stack used since [`paint_boot_stack`].
pub(crate) fn boot_stack_used() -> usize {
    let stack = boot::image().stack;
    let lo = mmu::phys_to_virt(stack.base);
    // SAFETY: the boot stack is mapped read-write for good.
    let s = unsafe { core::slice::from_raw_parts(lo as *const u8, stack.size) };
    stack.size - s.iter().take_while(|&&b| b == PAINT).count()
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
    // `memstat` reports where the memory goes (see `crate::memstat`).
    let memstat = has_bootarg(&info, "memstat");
    if memstat {
        paint_boot_stack();
    }
    let ram = check_board(&info);
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
    let reserved = [initrd.unwrap_or(none), dtb_region];
    let heap = init_heap(ram, &reserved);
    heap_test(&heap);
    let table_pages = rebuild_tables(ram);
    let grown = grow_heap(ram, &reserved);
    if memstat {
        crate::memstat::enable();
        let img = boot::image();
        println!(
            "kernel: mem ram {} KiB; image: text {} rodata {} data+bss {} stack {} KiB; heap {} KiB, {} KiB of it page tables",
            ram.size >> 10,
            img.text.size >> 10,
            img.rodata.size >> 10,
            img.data.size >> 10,
            img.stack.size >> 10,
            (heap.iter().map(|r| r.size).sum::<usize>() + grown) >> 10,
            (table_pages * PAGE_SIZE) >> 10
        );
    }
    irq::init(&info);
    irq::wait_ticks(10);
    println!("kernel: timer interrupts at {} Hz", irq::TICK_HZ);
    irq::alternation_test();
    println!("kernel: two threads alternated under timer preemption");
    let cons = console::init(&info, Input::Blocking);
    selftest::lock_test();
    selftest::tsleep_test();
    // The console test needs `xtask kernel` typing its input, so it
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
    let mode = match (test, has_bootarg(&info, "diskcheck")) {
        (_, true) => Mode::DiskCheck,
        (true, false) => Mode::Test {
            memstat: has_bootarg(&info, "memstat"),
            sh: bootarg(&info, "sh"),
        },
        (false, false) => Mode::Interactive,
    };
    println!("kernel: root {from}");
    let net = system::net::open(&fdt, &info);
    let host = system::host9p::find(&fdt);
    let blks = system::disk::find(&fdt);
    let rtc = info.rtc.map(|r| r.base as usize);
    let failed = system::start(archive, cons, rtc, net, host, blks, mode);
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
