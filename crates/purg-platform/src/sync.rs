//! Lock facade. With the `std` feature the locks are `std::sync` (blocking,
//! futex/ulock-backed); without it they are spinlocks, which is what the
//! bare-metal probe and kernel use. Both use acquire/release ordering, so
//! code is correct on weakly ordered aarch64.
//!
//! Poisoning is not exposed: a panic while holding a lock aborts the prog,
//! not the namespace, so the data is still used.

use core::ops::{Deref, DerefMut};

#[cfg(feature = "std")]
mod imp {
    pub type Mutex<T> = std::sync::Mutex<T>;
    pub type MutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;
    pub type RwLock<T> = std::sync::RwLock<T>;
    pub type ReadGuard<'a, T> = std::sync::RwLockReadGuard<'a, T>;
    pub type WriteGuard<'a, T> = std::sync::RwLockWriteGuard<'a, T>;

    pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn read<T: ?Sized>(l: &RwLock<T>) -> ReadGuard<'_, T> {
        l.read().unwrap_or_else(|e| e.into_inner())
    }
    pub fn write<T: ?Sized>(l: &RwLock<T>) -> WriteGuard<'_, T> {
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

    pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock()
    }
    pub fn read<T: ?Sized>(l: &RwLock<T>) -> ReadGuard<'_, T> {
        l.read()
    }
    pub fn write<T: ?Sized>(l: &RwLock<T>) -> WriteGuard<'_, T> {
        l.write()
    }
}

/// A mutual-exclusion lock.
#[derive(Default)]
pub struct Mutex<T>(imp::Mutex<T>);

pub struct MutexGuard<'a, T>(imp::MutexGuard<'a, T>);

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Mutex(imp::Mutex::new(v))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        MutexGuard(imp::lock(&self.0))
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
    pub const fn new(v: T) -> Self {
        RwLock(imp::RwLock::new(v))
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        ReadGuard(imp::read(&self.0))
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        WriteGuard(imp::write(&self.0))
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
