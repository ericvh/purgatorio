//! Lock and sleep/wakeup facade. With the `std` feature the locks are
//! `std::sync` (blocking, futex/ulock-backed) and [`Rendez`] parks the OS
//! thread on a `Condvar`. Without it the locks are spinlocks that disable
//! preemption while held and block when contended, and [`Rendez`]
//! blocks in the kernel scheduler registered with [`set_sched`], which is
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

/// Without `std` the locks are spinlocks that cooperate with the kernel
/// scheduler registered with [`set_sched`]:
///
/// - Holding a lock disables preemption ([`Sched::preempt_disable`]), so the
///   timer never switches a holder out in the middle of its critical
///   section: the scheduler defers the switch until the thread's last lock
///   is released. Interrupts stay enabled, so interrupt latency does not
///   grow with the length of critical sections.
/// - A contended lock does not spin. On one CPU its holder is not running,
///   so the waiter re-enables preemption and blocks ([`Sched::block`])
///   until the holder releases the lock and wakes it. This also keeps a
///   holder that sleeps with the lock held from deadlocking its waiters:
///   these locks are Inferno qlocks, and `purg-core` does sleep holding
///   some (the `#c/cons` line buffer, for one).
///
/// With no scheduler registered the hooks do nothing and a contended lock
/// spins, as before.
#[cfg(not(feature = "std"))]
mod imp {
    use core::mem::ManuallyDrop;
    use core::ops::{Deref, DerefMut};
    use core::sync::atomic::{AtomicBool, Ordering};

    fn preempt_disable() {
        if let Some(s) = super::SCHED.get() {
            s.preempt_disable();
        }
    }

    fn preempt_enable() {
        if let Some(s) = super::SCHED.get() {
            s.preempt_enable();
        }
    }

    /// Takes a lock with `try_lock`. Preemption is disabled from just
    /// before the successful attempt until the guard drops. Between failed
    /// attempts the caller blocks, having recorded in `waiters` that the
    /// holder must wake it.
    fn acquire<G>(waiters: &AtomicBool, mut try_lock: impl FnMut() -> Option<G>) -> G {
        loop {
            preempt_disable();
            if let Some(g) = try_lock() {
                return g;
            }
            waiters.store(true, Ordering::Release);
            // A release between the failed attempt and the store did not
            // see `waiters`; try again before blocking.
            if let Some(g) = try_lock() {
                return g;
            }
            preempt_enable();
            match super::SCHED.get() {
                Some(s) => s.block(),
                None => core::hint::spin_loop(),
            }
        }
    }

    /// Called after the lock word is released.
    fn release(waiters: &AtomicBool) {
        if waiters.swap(false, Ordering::AcqRel)
            && let Some(s) = super::SCHED.get()
        {
            s.wakeup();
        }
        preempt_enable();
    }

    /// A spin guard that, when it drops, releases the lock, wakes waiters
    /// and re-enables preemption, in that order.
    pub struct Guard<'a, G> {
        inner: ManuallyDrop<G>,
        waiters: &'a AtomicBool,
    }

    impl<'a, G> Guard<'a, G> {
        fn new(inner: G, waiters: &'a AtomicBool) -> Self {
            Guard {
                inner: ManuallyDrop::new(inner),
                waiters,
            }
        }
    }

    impl<G> Drop for Guard<'_, G> {
        fn drop(&mut self) {
            // SAFETY: `inner` is dropped exactly once, here, and not used
            // after.
            unsafe { ManuallyDrop::drop(&mut self.inner) };
            release(self.waiters);
        }
    }

    impl<G: Deref> Deref for Guard<'_, G> {
        type Target = G::Target;
        fn deref(&self) -> &G::Target {
            &self.inner
        }
    }

    impl<G: DerefMut> DerefMut for Guard<'_, G> {
        fn deref_mut(&mut self) -> &mut G::Target {
            &mut self.inner
        }
    }

    #[derive(Default)]
    pub struct Mutex<T> {
        lock: spin::Mutex<T>,
        waiters: AtomicBool,
    }

    impl<T> Mutex<T> {
        pub const fn new(v: T) -> Self {
            Mutex {
                lock: spin::Mutex::new(v),
                waiters: AtomicBool::new(false),
            }
        }

        pub fn into_inner(self) -> T {
            self.lock.into_inner()
        }
    }

    pub type MutexGuard<'a, T> = Guard<'a, spin::MutexGuard<'a, T>>;

    #[derive(Default)]
    pub struct RwLock<T> {
        lock: spin::RwLock<T>,
        waiters: AtomicBool,
    }

    impl<T> RwLock<T> {
        pub const fn new(v: T) -> Self {
            RwLock {
                lock: spin::RwLock::new(v),
                waiters: AtomicBool::new(false),
            }
        }
    }

    pub type ReadGuard<'a, T> = Guard<'a, spin::RwLockReadGuard<'a, T>>;
    pub type WriteGuard<'a, T> = Guard<'a, spin::RwLockWriteGuard<'a, T>>;

    pub mod lockops {
        use super::*;

        pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
            Guard::new(acquire(&m.waiters, || m.lock.try_lock()), &m.waiters)
        }
        pub fn read<T>(l: &RwLock<T>) -> ReadGuard<'_, T> {
            Guard::new(acquire(&l.waiters, || l.lock.try_read()), &l.waiters)
        }
        pub fn write<T>(l: &RwLock<T>) -> WriteGuard<'_, T> {
            Guard::new(acquire(&l.waiters, || l.lock.try_write()), &l.waiters)
        }
    }
}

#[cfg(not(feature = "std"))]
use imp::lockops;

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
    /// Called by a thread that cannot make progress until another thread
    /// or an interrupt changes something: a [`Rendez`] sleeper whose
    /// condition is false, or a waiter for a held lock. Like
    /// [`Sched::yield_now`], but when every thread is blocked the scheduler
    /// may idle the CPU until the next interrupt or [`Sched::wakeup`].
    fn block(&self) {
        self.yield_now();
    }
    /// Something a blocked thread may be waiting for has changed:
    /// [`Rendez::wakeup`] or the release of a lock that has waiters. Must
    /// be safe to call from an interrupt handler.
    fn wakeup(&self) {}
    /// Disables preemption of the running thread; nests. Called when it
    /// takes a lock. A voluntary switch ([`Sched::yield_now`],
    /// [`Sched::block`]) is still allowed and keeps the count with the
    /// thread.
    fn preempt_disable(&self) {}
    /// Undoes one [`Sched::preempt_disable`]. When the count drops to zero
    /// a preemption deferred meanwhile happens now.
    fn preempt_enable(&self) {}
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
                    Some(s) => s.block(),
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
                // The scheduler's clock interrupt ends an idle block, so
                // the deadline is seen within a tick.
                s.block();
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
        // Sleepers poll their conditions; this tells an idle scheduler to
        // let them.
        #[cfg(not(feature = "std"))]
        if let Some(s) = SCHED.get() {
            s.wakeup();
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
