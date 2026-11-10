//! `kernel_main`, the console, the heap, exit, and fatal error reporting.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::asm;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};

use linked_list_allocator::LockedHeap;
use purg_aarch64::dtb::{self, BootInfo, Fdt};
use purg_aarch64::mmu::{self, KERNEL_VBASE, PAGE_SIZE, Region, TableAlloc};
use purg_aarch64::pl011::Pl011;
use purg_aarch64::semihost;

use crate::board::{ExitMethod, board, dtb_size, exit_method};
use crate::boot::{self, KERNEL_BOOT_EL};
use crate::irq;

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

/// The console: the PL011 through the high-half device window. Writes are
/// unlocked: there is one core, and only the boot thread prints.
pub struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // SAFETY: the high half maps the device window, which holds the
        // PL011, as Device memory.
        let uart = unsafe { Pl011::new(mmu::phys_to_virt(board().uart)) };
        uart.write(s.as_bytes());
        Ok(())
    }
}

macro_rules! println {
    ($($arg:tt)*) => {{
        let _ = writeln!(Console, $($arg)*);
    }};
}

const EXIT_SEMIHOSTING: u8 = 0;
const EXIT_PSCI_HVC: u8 = 1;
const EXIT_PSCI_SMC: u8 = 2;

static EXIT: AtomicU8 = AtomicU8::new(EXIT_SEMIHOSTING);

/// Leaves QEMU. Semihosting passes `code` on; PSCI `SYSTEM_OFF` cannot, so
/// callers print `kernel: ok` or `kernel: FAILED ...` first.
fn exit(code: u32) -> ! {
    const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
    match EXIT.load(Ordering::Acquire) {
        // SAFETY: PSCI SYSTEM_OFF does not return; the conduit comes from
        // the DTB.
        EXIT_PSCI_HVC => unsafe { asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(nostack)) },
        // SAFETY: as above.
        EXIT_PSCI_SMC => unsafe { asm!("smc #0", in("x0") PSCI_SYSTEM_OFF, options(nostack)) },
        _ => semihost::exit(code),
    }
    loop {
        // SAFETY: parks the core if SYSTEM_OFF returned.
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

pub fn fail(args: fmt::Arguments<'_>) -> ! {
    println!("kernel: FAILED {args}");
    exit(1)
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

/// Finds the DTB: QEMU passes its address in x0 for a Linux-style boot and
/// 0 for a bare-metal ELF, when it is at the start of RAM. Returns the
/// blob and whether it came from x0.
fn find_dtb(x0: usize) -> (&'static [u8], bool) {
    let (pa, from_x0) = if x0 != 0 {
        (x0, true)
    } else {
        (board().ram.base, false)
    };
    let va = mmu::phys_to_virt(pa);
    // SAFETY: the linear map covers RAM; the header is 40 bytes.
    let head = unsafe { core::slice::from_raw_parts(va as *const u8, 40) };
    let Some(size) = dtb_size(head) else {
        fail(format_args!("no DTB at {pa:#x}"));
    };
    check!(
        pa + size <= board().ram.end() && pa >= board().ram.base,
        "DTB at {pa:#x} is outside RAM"
    );
    // SAFETY: checked to be inside the linear-mapped RAM.
    (
        unsafe { core::slice::from_raw_parts(va as *const u8, size) },
        from_x0,
    )
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

fn init_heap() -> Region {
    let ram = board().ram;
    let start = boot::image_end();
    let heap = Region {
        base: start,
        size: ram.end() - start,
    };
    // SAFETY: RAM after the image is unused and linear-mapped RW.
    unsafe {
        HEAP.lock()
            .init(mmu::phys_to_virt(heap.base) as *mut u8, heap.size);
    }
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

/// The kernel entry point, in the high half with the MMU on. `dtb_phys` is
/// x0 at boot: the DTB's physical address, or 0 if QEMU did not pass one.
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(dtb_phys: usize) -> ! {
    let (dtb, from_x0) = find_dtb(dtb_phys);
    let fdt = match Fdt::new(dtb) {
        Ok(f) => f,
        Err(e) => fail(format_args!("bad DTB: {e:?}")),
    };
    let info = match BootInfo::from_fdt(&fdt) {
        Ok(i) => i,
        Err(e) => fail(format_args!("DTB: {e:?}")),
    };
    let method = exit_method(&fdt, &info);
    let code = match method {
        ExitMethod::Semihosting => EXIT_SEMIHOSTING,
        ExitMethod::Psci { smc: false } => EXIT_PSCI_HVC,
        ExitMethod::Psci { smc: true } => EXIT_PSCI_SMC,
    };
    EXIT.store(code, Ordering::Release);

    println!("Purgatorio kernel {} (aarch64)", env!("CARGO_PKG_VERSION"));
    println!(
        "kernel: booted at EL{}",
        KERNEL_BOOT_EL.load(Ordering::Acquire)
    );
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
    let main = kernel_main as *const () as usize;
    check!(
        pc >= KERNEL_VBASE && sp >= KERNEL_VBASE && main >= KERNEL_VBASE,
        "not in the high half: pc {pc:#x} sp {sp:#x} kernel_main {main:#x}"
    );
    println!("kernel: running in the high half");
    check_board(&info);
    check_sections();
    let heap = init_heap();
    heap_test(heap);
    rebuild_tables();
    irq::init(&info);
    irq::wait_ticks(10);
    println!("kernel: timer interrupts at {} Hz", irq::TICK_HZ);
    irq::alternation_test();
    println!("kernel: two threads alternated under timer preemption");
    println!("kernel: ok");
    exit(0)
}

/// Called by the vector table on any exception: none is expected yet.
#[unsafe(no_mangle)]
extern "C" fn kernel_exception(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    fail(format_args!(
        "exception {kind} esr={esr:#x} elr={elr:#x} far={far:#x}"
    ))
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    fail(format_args!("panic: {info}"))
}
