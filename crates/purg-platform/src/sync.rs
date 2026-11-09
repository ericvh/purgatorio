//! Lock and sleep/wakeup facade. With the `std` feature the locks are
//! `std::sync` (blocking, futex/ulock-backed) and [`Rendez`] parks the OS
//! thread on a `Condvar`. Without it the locks are spinlocks and [`Rendez`]
//! yields to the kernel scheduler registered with [`set_sched`], which is
//! what the bare-metal probe and kernel use. Both use acquire/release
//! ordering, so code is correct on weakly ordered aarch64.
//!
//! Built with `--cfg loom`, the `std` backend uses loom's model-checked
//! primitives so `Rendez` and the queues built on it can be checked
//! exhaustively.
//!
//! Poisoning is not exposed: a panic while holding a lock aborts the prog,
//! not the namespace, so the data is still used.

use core::ops::{Deref, DerefMut};

#[cfg(all(feature = "std", not(loom)))]
mod imp {
    pub use std::sync::{
        Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard as ReadGuard,
        RwLockWriteGuard as WriteGuard,
    };
}

#[cfg(all(feature = "std", loom))]
mod imp {
    pub use loom::sync::{
        Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard as ReadGuard,
        RwLockWriteGuard as WriteGuard,
    };
}

#[cfg(feature = "std")]
mod lockops {
    use super::imp::*;

    pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn read<T>(l: &RwLock<T>) -> ReadGuard<'_, T> {
        l.read().unwrap_or_else(|e| e.into_inner())
    }
    pub fn write<T>(l: &RwLock<T>) -> WriteGuard<'_, T> {
        l.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(not(feature = "std"))]
mod imp {
    pub type Mutex<T> = spin::Mutex<T>;
    pub type MutexGuard<'a, T> = spin::MutexGuard<'a, T>;
    pub type RwLock<T> = spin::RwLock<T>;
    pub type ReadGuard<'a, T> = spin::RwLockReadGuard<'a, T>;
    pub type WriteGuard<'a, T> = spin::RwLockWriteGuard<'a, T>;
}

#[cfg(not(feature = "std"))]
mod lockops {
    use super::imp::*;

    pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock()
    }
    pub fn read<T>(l: &RwLock<T>) -> ReadGuard<'_, T> {
        l.read()
    }
    pub fn write<T>(l: &RwLock<T>) -> WriteGuard<'_, T> {
        l.write()
    }
}

/// A mutual-exclusion lock.
#[derive(Default)]
pub struct Mutex<T>(imp::Mutex<T>);

pub struct MutexGuard<'a, T>(imp::MutexGuard<'a, T>);

impl<T> Mutex<T> {
    pub fn new(v: T) -> Self {
        Mutex(imp::Mutex::new(v))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        MutexGuard(lockops::lock(&self.0))
    }

    pub fn into_inner(self) -> T {
        #[cfg(feature = "std")]
        {
            self.0.into_inner().unwrap_or_else(|e| e.into_inner())
        }
        #[cfg(not(feature = "std"))]
        {
            self.0.into_inner()
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// A reader-writer lock.
#[derive(Default)]
pub struct RwLock<T>(imp::RwLock<T>);

pub struct ReadGuard<'a, T>(imp::ReadGuard<'a, T>);
pub struct WriteGuard<'a, T>(imp::WriteGuard<'a, T>);

impl<T> RwLock<T> {
    pub fn new(v: T) -> Self {
        RwLock(imp::RwLock::new(v))
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        ReadGuard(lockops::read(&self.0))
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        WriteGuard(lockops::write(&self.0))
    }
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// The kernel scheduler hooks [`Rendez`] uses without `std`.
pub trait Sched: Sync {
    /// Lets other kernel threads run; returns when this one is scheduled
    /// again.
    fn yield_now(&self);
    /// Monotonic time in nanoseconds, for [`Rendez::tsleep`].
    fn monotonic_ns(&self) -> u64;
    /// The [`Note`] of the kernel thread that is running.
    fn note(&self) -> Option<alloc::sync::Arc<Note>> {
        None
    }
    /// Records the [`Note`] of the kernel thread that is running.
    fn set_note(&self, _note: Option<alloc::sync::Arc<Note>>) {}
}

#[cfg(not(feature = "std"))]
static SCHED: spin::Once<&'static dyn Sched> = spin::Once::new();

/// Registers the kernel scheduler. Before this, sleeping spins.
#[cfg(not(feature = "std"))]
pub fn set_sched(s: &'static dyn Sched) {
    SCHED.call_once(|| s);
}

/// Inferno's `Rendez`: a place to sleep until a condition holds.
///
/// The condition is always evaluated with the Rendez's lock held, and
/// [`Rendez::wakeup`] takes that lock, so a waker that changes the state
/// the condition reads and then calls `wakeup` can never be missed.
///
/// Cloning a `Rendez` clones a handle to the same sleepers, which is what
/// [`Note`] stores so `kill` can wake the prog that is parked here.
pub struct Rendez {
    inner: alloc::sync::Arc<RendezInner>,
}

struct RendezInner {
    #[cfg(feature = "std")]
    lock: imp::Mutex<()>,
    #[cfg(feature = "std")]
    cv: imp::Condvar,
}

impl Clone for Rendez {
    fn clone(&self) -> Rendez {
        Rendez {
            inner: self.inner.clone(),
        }
    }
}

impl Default for Rendez {
    fn default() -> Rendez {
        Rendez::new()
    }
}

impl Rendez {
    pub fn new() -> Rendez {
        Rendez {
            inner: alloc::sync::Arc::new(RendezInner {
                #[cfg(feature = "std")]
                lock: imp::Mutex::new(()),
                #[cfg(feature = "std")]
                cv: imp::Condvar::new(),
            }),
        }
    }

    /// Whether `other` parks the same sleepers.
    pub fn same(&self, other: &Rendez) -> bool {
        alloc::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Sleeps until `cond` returns true.
    pub fn sleep(&self, mut cond: impl FnMut() -> bool) {
        #[cfg(feature = "std")]
        {
            let mut g = lockops::lock(&self.inner.lock);
            while !cond() {
                g = self.inner.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        #[cfg(not(feature = "std"))]
        {
            while !cond() {
                match SCHED.get() {
                    Some(s) => s.yield_now(),
                    None => core::hint::spin_loop(),
                }
            }
        }
    }

    /// Sleeps until `cond` returns true or `timeout_ns` passes. Returns the
    /// final value of `cond`.
    pub fn tsleep(&self, mut cond: impl FnMut() -> bool, timeout_ns: u64) -> bool {
        #[cfg(all(feature = "std", not(loom)))]
        {
            let deadline = std::time::Instant::now() + core::time::Duration::from_nanos(timeout_ns);
            let mut g = lockops::lock(&self.inner.lock);
            loop {
                if cond() {
                    return true;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return false;
                }
                g = self
                    .inner
                    .cv
                    .wait_timeout(g, deadline - now)
                    .map(|(g, _)| g)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
        }
        #[cfg(all(feature = "std", loom))]
        {
            // loom has no time; a timed sleep is a single check.
            let _ = timeout_ns;
            let _g = lockops::lock(&self.inner.lock);
            cond()
        }
        #[cfg(not(feature = "std"))]
        {
            let Some(s) = SCHED.get() else {
                return cond();
            };
            let deadline = s.monotonic_ns().saturating_add(timeout_ns);
            loop {
                if cond() {
                    return true;
                }
                if s.monotonic_ns() >= deadline {
                    return false;
                }
                s.yield_now();
            }
        }
    }

    /// Wakes every sleeper so it re-evaluates its condition.
    pub fn wakeup(&self) {
        #[cfg(feature = "std")]
        {
            drop(lockops::lock(&self.inner.lock));
            self.inner.cv.notify_all();
        }
    }
}

#[cfg(not(loom))]
use core::sync::atomic::{AtomicBool, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, Ordering};

/// A prog's kill flag and the [`Rendez`] it is parked on.
///
/// `kill` sets the flag and wakes that rendez, so a sleeper whose condition
/// also watches [`Note::killed`] cannot miss it: the condition is rechecked
/// under the rendez lock, and `wakeup` takes the same lock.
pub struct Note {
    killed: AtomicBool,
    sleep: Mutex<Option<Rendez>>,
}

impl Note {
    pub fn new() -> Note {
        Note {
            killed: AtomicBool::new(false),
            sleep: Mutex::new(None),
        }
    }

    pub fn killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    /// Makes `kill` wake `r`. Paired with [`Note::disarm`] around a timed sleep
    /// that cannot register itself.
    pub fn arm(&self, r: &Rendez) {
        *self.sleep.lock() = Some(r.clone());
    }

    /// Forgets the rendez [`Note::arm`] recorded.
    pub fn disarm(&self) {
        *self.sleep.lock() = None;
    }

    /// Marks the prog killed and wakes it if it is parked.
    pub fn kill(&self) {
        self.killed.store(true, Ordering::Release);
        if let Some(r) = self.sleep.lock().clone() {
            r.wakeup();
        }
    }

    /// Sleeps on `r` until `cond` is true. Returns whether `cond` became
    /// true; `false` means the note was killed first. `cond` may run more
    /// than once and must not sleep.
    pub fn sleep(&self, r: &Rendez, mut cond: impl FnMut() -> bool) -> bool {
        if cond() {
            return true;
        }
        if self.killed() {
            return false;
        }
        *self.sleep.lock() = Some(r.clone());
        r.sleep(|| self.killed() || cond());
        *self.sleep.lock() = None;
        cond()
    }
}

impl Default for Note {
    fn default() -> Note {
        Note::new()
    }
}

#[cfg(feature = "std")]
mod current {
    use super::Note;
    use alloc::sync::Arc;
    use std::cell::RefCell;
    use std::thread_local;

    thread_local! {
        static CURRENT: RefCell<Option<Arc<Note>>> = const { RefCell::new(None) };
    }

    pub fn set(note: Option<Arc<Note>>) {
        CURRENT.with(|c| *c.borrow_mut() = note);
    }

    pub fn get() -> Option<Arc<Note>> {
        CURRENT.with(|c| c.borrow().clone())
    }
}

#[cfg(not(feature = "std"))]
mod current {
    use super::{Note, SCHED};
    use alloc::sync::Arc;

    pub fn set(note: Option<Arc<Note>>) {
        if let Some(s) = SCHED.get() {
            s.set_note(note);
        }
    }

    pub fn get() -> Option<Arc<Note>> {
        SCHED.get().and_then(|s| s.note())
    }
}

/// Records the note of the prog running on this thread (or kernel thread).
pub fn set_current_note(note: Option<alloc::sync::Arc<Note>>) {
    current::set(note);
}

/// The note recorded for this thread, if a prog is running on it.
pub fn current_note() -> Option<alloc::sync::Arc<Note>> {
    current::get()
}
