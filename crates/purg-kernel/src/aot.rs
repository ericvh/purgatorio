//! The experimental AOT engine (`--features aot`, `docs/perf.md`): modules
//! compiled on the build host by `purg-vm-wasmtime`'s `cwasm` tool run on
//! wasmtime's `no_std` runtime (`purg_vm_aot`), the rest on wasmi.
//!
//! The image set is linked into the kernel's text (`.text.aot`), which
//! the MMU maps read-only and executable, so native code needs no page
//! made executable at run time and nothing outside the image ever runs
//! as code. `xtask bench --aot` builds the set and names it in
//! `PURG_AOT_IMAGES`; the build script copies it here. Without one the
//! set is empty and every module runs on wasmi.
//!
//! This module also supplies wasmtime's platform hooks: one pointer of
//! thread-local storage per prog (wasm runs only on a prog's thread, so
//! the pid names the thread), and the locks of its
//! `custom-sync-primitives`, which spin and yield.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use purg_core::Error;
use purg_platform::sync::{Mutex, Rendez, Sched};
use purg_vm_aot::{ALIGN, Aot, Images};

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

/// The engine: the AOT images with `fallback` for the rest, or
/// `fallback` alone when the kernel has no images.
pub fn engine(fallback: Arc<dyn purg_vm::Engine>) -> Result<Arc<dyn purg_vm::Engine>, Error> {
    let images = match Images::parse(&SET.0) {
        Ok(i) if !i.is_empty() => i,
        _ => {
            println!("kernel: aot: no images, wasmi only");
            return Ok(fallback);
        }
    };
    println!(
        "kernel: aot: {} {:?} images in {} KiB",
        images.len(),
        images.kind(),
        SET.0.len() >> 10
    );
    let aot = Arc::new(Aot::new(images, fallback)?);
    let ticker = aot.clone();
    SCHED.spawn(alloc::boxed::Box::new(move || {
        let r = Rendez::new();
        loop {
            r.tsleep(|| false, TICK_NS);
            ticker.tick();
        }
    }));
    Ok(aot)
}

static TLS: spin::Once<Mutex<BTreeMap<u32, usize>>> = spin::Once::new();

fn tls() -> &'static Mutex<BTreeMap<u32, usize>> {
    TLS.call_once(|| Mutex::new(BTreeMap::new()))
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get(slot: usize) -> *mut u8 {
    if slot != 0 {
        return core::ptr::null_mut();
    }
    let pid = SCHED.current_pid();
    tls().lock().get(&pid).copied().unwrap_or(0) as *mut u8
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(slot: usize, ptr: *mut u8) {
    if slot != 0 {
        return;
    }
    let pid = SCHED.current_pid();
    let mut t = tls().lock();
    if ptr.is_null() {
        t.remove(&pid);
    } else {
        t.insert(pid, ptr as usize);
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
