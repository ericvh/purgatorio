//! The AOT engine (`--features aot`, `docs/perf.md`): modules compiled on
//! the build host by `purg-cwasm`'s `cwasm` tool run on wasmtime's
//! `no_std` runtime (`purg_vm_aot`), the rest on wasmi.
//!
//! Images come from two places. A set linked into the kernel's text
//! (`.text.aot`, which the MMU maps read-only and executable):
//! `PURG_AOT_IMAGES` names it at build time (`xtask bench --aot`) and the
//! build script copies it here; without one the set is empty. And signed
//! image files beside the modules (`/bin/<cmd>.cwasm`), which run only if
//! a key in `/lib/keys/aot` of the boot namespace signed them for that
//! module (`docs/signing.md`); their code goes into the code region
//! (`purg_aarch64::code`), written and then made executable.
//!
//! The `engine=` boot argument chooses: `wasmi` runs every module on
//! wasmi, `auto` (the default) runs a module on wasmtime where it has a
//! verified image, and `aot` refuses a module without one (a traced prog
//! still runs on wasmi).
//!
//! This module also supplies wasmtime's platform hooks: one pointer of
//! thread-local storage per kernel thread, a word in the scheduler's
//! thread (`purg_aarch64::sched::tls`), and the locks of its
//! `custom-sync-primitives`, which spin and yield.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::alloc::Layout;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use purg_core::{Error, Pgrp};
use purg_platform::sync::{Rendez, Sched};
use purg_vm_aot::{ALIGN, Aot, Code, Hooks, Images, Kind, Mode, Sources};

use crate::irq::SCHED;
use crate::kernel::println;

#[repr(C, align(65536))]
struct Aligned<T>(T);

const _: () = assert!(ALIGN == 65536);

#[unsafe(link_section = ".text.aot")]
static SET: Aligned<[u8; include_bytes!(concat!(env!("OUT_DIR"), "/aot.bin")).len()]> =
    Aligned(*include_bytes!(concat!(env!("OUT_DIR"), "/aot.bin")));

/// How often running guests check their note.
const TICK_NS: u64 = 10_000_000;

/// The `engine=` boot argument: 0 `auto`, 1 `aot`, 2 `wasmi`.
static ENGINE: AtomicU8 = AtomicU8::new(0);

/// Reads the `engine=` boot argument. Returns false for a value that is
/// not `wasmi`, `aot` or `auto`, which leaves `auto`.
pub fn set_engine(arg: Option<&str>) -> bool {
    let v = match arg {
        None | Some("auto") => 0,
        Some("aot") => 1,
        Some("wasmi") => 2,
        Some(_) => return false,
    };
    ENGINE.store(v, Ordering::Relaxed);
    true
}

/// The engine, once made, for [`report`].
static AOT: spin::Once<Arc<Aot>> = spin::Once::new();

/// The engine: wasmtime for modules with images, with `fallback` for the
/// rest, or `fallback` alone under `engine=wasmi`. `boot` is the boot
/// namespace, whose `/lib/keys/aot` vouches for image files.
pub fn engine(
    fallback: Arc<dyn purg_vm::Engine>,
    boot: &Arc<Pgrp>,
) -> Result<Arc<dyn purg_vm::Engine>, Error> {
    let mode = match ENGINE.load(Ordering::Relaxed) {
        2 => {
            println!("kernel: aot: engine=wasmi");
            return Ok(fallback);
        }
        1 => Mode::Only,
        _ => Mode::Auto,
    };
    let linked = Images::parse(&SET.0).ok().filter(|i| !i.is_empty());
    let kind = linked.as_ref().map_or(Kind::Native, Images::kind);
    println!(
        "kernel: aot: engine={}, {} {kind:?} images linked in",
        if mode == Mode::Only { "aot" } else { "auto" },
        linked.as_ref().map_or(0, Images::len),
    );
    let sources = Sources {
        kind,
        linked,
        // A copy: progs that run in the boot namespace itself, such as
        // the first sh, bind over /lib/keys there.
        trust: Some(Arc::new(boot.fork())),
    };
    let aot = Arc::new(Aot::new(sources, mode, Arc::new(Kernel), fallback)?);
    let ticker = aot.clone();
    SCHED.spawn(Box::new(move || {
        let r = Rendez::new();
        loop {
            r.tsleep(|| false, TICK_NS);
            ticker.tick();
        }
    }));
    AOT.call_once(|| aot.clone());
    Ok(aot)
}

/// One line on how many progs ran compiled and how much code is loaded,
/// at the end of the test mode.
pub fn report() {
    if let Some(aot) = AOT.get() {
        let (native, interpreted) = aot.counts();
        let (pages, chunks) = purg_aarch64::code::stats();
        println!(
            "kernel: aot: {native} progs compiled, {interpreted} on wasmi; code region {pages} pages in {chunks} chunks"
        );
    }
}

/// The kernel side of the engine.
struct Kernel;

struct Loaded(purg_aarch64::code::Code);

impl Code for Loaded {
    fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }
}

/// A stack for guest code, from the heap. It has no guard page, but
/// wasmtime checks the stack in every function's prologue, and the host
/// calls under the deepest frame stay within [`purg_vm_aot::HOST_STACK`].
struct Stack(*mut u8, Layout);

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: allocated in `on_stack` with this layout.
        unsafe { alloc::alloc::dealloc(self.0, self.1) };
    }
}

impl Hooks for Kernel {
    fn on_stack(&self, size: usize, f: &mut dyn FnMut()) {
        let Ok(layout) = Layout::from_size_align(size, 16) else {
            return;
        };
        // SAFETY: a non-zero-size layout. A stack need not be zeroed.
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return;
        }
        let stack = Stack(p, layout);
        // SAFETY: the block is ours for the call and `size` bytes long.
        let s = unsafe { core::slice::from_raw_parts_mut(stack.0, size) };
        purg_aarch64::sched::on_stack(s, f);
    }

    fn load_code(&self, image: &[u8]) -> Result<Box<dyn Code>, Error> {
        match purg_aarch64::code::load(image) {
            Ok(c) => Ok(Box::new(Loaded(c))),
            Err(e) => Err(Error::from(e.as_str())),
        }
    }

    fn is_code(&self, ptr: *const u8, len: usize) -> bool {
        purg_aarch64::code::contains(ptr, len)
    }

    fn sync_code(&self) {
        purg_aarch64::code::sync();
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get(slot: usize) -> *mut u8 {
    // Slot 1 is the component model's, which this runtime leaves out.
    if slot != 0 {
        return core::ptr::null_mut();
    }
    purg_aarch64::sched::tls() as *mut u8
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(slot: usize, ptr: *mut u8) {
    if slot == 0 {
        purg_aarch64::sched::set_tls(ptr as usize);
    }
}

/// A lock word: 0 free, [`WRITER`] held by a writer (or by the one owner
/// of a mutex), otherwise the number of readers.
const WRITER: usize = usize::MAX;

/// # Safety
///
/// wasmtime passes a pointer to a lock word it owns, zero when unused.
unsafe fn word<'a>(lock: *mut usize) -> &'a AtomicUsize {
    // SAFETY: the word is aligned, live and only used atomically.
    unsafe { AtomicUsize::from_ptr(lock) }
}

fn acquire(w: &AtomicUsize, from: impl Fn(usize) -> Option<usize>) {
    loop {
        let cur = w.load(Ordering::Relaxed);
        if let Some(next) = from(cur)
            && w.compare_exchange(cur, next, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            return;
        }
        SCHED.yield_now();
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_lock_free(_lock: *mut usize) {}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_lock_acquire(lock: *mut usize) {
    // SAFETY: see `word`.
    acquire(unsafe { word(lock) }, |c| (c == 0).then_some(WRITER));
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_lock_release(lock: *mut usize) {
    // SAFETY: see `word`.
    unsafe { word(lock) }.store(0, Ordering::Release);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_rwlock_read(lock: *mut usize) {
    // SAFETY: see `word`.
    acquire(unsafe { word(lock) }, |c| (c < WRITER - 1).then_some(c + 1));
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_rwlock_read_release(lock: *mut usize) {
    // SAFETY: see `word`.
    unsafe { word(lock) }.fetch_sub(1, Ordering::Release);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_rwlock_write(lock: *mut usize) {
    // SAFETY: see `word`.
    acquire(unsafe { word(lock) }, |c| (c == 0).then_some(WRITER));
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_rwlock_write_release(lock: *mut usize) {
    // SAFETY: see `word`.
    unsafe { word(lock) }.store(0, Ordering::Release);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_sync_rwlock_free(_lock: *mut usize) {}
