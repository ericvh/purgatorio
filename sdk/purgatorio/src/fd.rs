//! File descriptors that close on drop.
//!
//! In a wasm32 guest these are `std::os::fd`'s. A wasm64 guest has no WASI
//! and so no `std::os::fd`; there they are this module's own, with the
//! same names and the same meaning (`docs/wasm64.md`). Code that names
//! them through here builds for both widths.

#[cfg(not(target_arch = "wasm64"))]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

#[cfg(target_arch = "wasm64")]
pub use wide::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

#[cfg(target_arch = "wasm64")]
mod wide {
    pub type RawFd = i32;

    pub trait AsRawFd {
        fn as_raw_fd(&self) -> RawFd;
    }

    pub trait FromRawFd {
        /// # Safety
        /// `fd` must be open and owned by nothing else.
        unsafe fn from_raw_fd(fd: RawFd) -> Self;
    }

    pub trait IntoRawFd {
        fn into_raw_fd(self) -> RawFd;
    }

    /// An fd this value owns: it is closed when the value is dropped.
    #[derive(Debug)]
    pub struct OwnedFd(RawFd);

    impl OwnedFd {
        /// A second fd on the same file (`sys->dup`).
        pub fn try_clone(&self) -> std::io::Result<OwnedFd> {
            let n = unsafe { crate::sys::dup(self.0, -1) };
            if n < 0 {
                return Err(crate::io::last_error());
            }
            Ok(OwnedFd(n))
        }
    }

    impl AsRawFd for OwnedFd {
        fn as_raw_fd(&self) -> RawFd {
            self.0
        }
    }

    impl AsRawFd for RawFd {
        fn as_raw_fd(&self) -> RawFd {
            *self
        }
    }

    impl FromRawFd for OwnedFd {
        unsafe fn from_raw_fd(fd: RawFd) -> OwnedFd {
            OwnedFd(fd)
        }
    }

    impl IntoRawFd for OwnedFd {
        fn into_raw_fd(self) -> RawFd {
            let fd = self.0;
            core::mem::forget(self);
            fd
        }
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            unsafe { crate::sys::close(self.0) };
        }
    }
}
