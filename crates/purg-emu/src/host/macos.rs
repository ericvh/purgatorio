//! macOS-specific host code.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// APFS and HFS+ volumes are case-insensitive (and normalization-insensitive)
/// by default, but Inferno names are byte-exact. After a host lookup
/// succeeds, check that `name` exists in `dir` with exactly these bytes,
/// unless the volume reports itself case-sensitive.
pub fn exact_name_exists(dir: &Path, name: &str) -> io::Result<bool> {
    let cdir = CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: cdir is a valid NUL-terminated path.
    let sensitive = unsafe { libc::pathconf(cdir.as_ptr(), libc::_PC_CASE_SENSITIVE) };
    if sensitive == 1 {
        return Ok(true);
    }
    for entry in std::fs::read_dir(dir)? {
        if entry?.file_name().as_bytes() == name.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Finder metadata and AppleDouble files are hidden from `#U` listings.
pub fn hidden_entry(name: &str) -> bool {
    name == ".DS_Store" || name.starts_with("._")
}

/// `(max resident bytes, physical memory)`. `ru_maxrss` is bytes on macOS.
pub fn memory_usage() -> (u64, u64) {
    let mut usage = unsafe { core::mem::zeroed::<libc::rusage>() };
    let used = if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
        usage.ru_maxrss as u64
    } else {
        0
    };
    let mut total: u64 = 0;
    let mut len = core::mem::size_of::<u64>();
    let key = CString::new("hw.memsize").unwrap_or_else(|_| CString::default());
    unsafe {
        libc::sysctlbyname(
            key.as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut len,
            core::ptr::null_mut(),
            0,
        );
    }
    (used, total)
}

/// Why an ICMP socket could not be opened when the host refuses it.
pub const ICMP_DENIED: &str =
    "permission denied: this host does not allow unprivileged ICMP sockets";

/// Whether the host replaces an ICMP datagram socket's echo identifier
/// with its own and delivers only the replies that match it. macOS does
/// not: every socket sees every echo reply, and the emu sorts them.
pub const ICMP_IDENT_BY_HOST: bool = false;

/// An unprivileged ICMP datagram socket (`SOCK_DGRAM`, `IPPROTO_ICMP`),
/// which macOS allows to every user.
pub fn icmp_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the result is checked.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor this function owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl on a descriptor we own.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// One ICMP message received: it is `buf[start..start + len]`.
pub struct IcmpIn {
    pub start: usize,
    pub len: usize,
    pub ttl: u8,
    pub src: Ipv4Addr,
}

/// Receives one message on an [`icmp_socket`]. macOS delivers the IPv4
/// header in front of the ICMP message, so the TTL and source are read
/// from it.
pub fn icmp_recv(fd: &impl AsRawFd, buf: &mut [u8]) -> io::Result<IcmpIn> {
    // SAFETY: buf is valid for its length; no address is asked for.
    let n = unsafe {
        libc::recvfrom(
            fd.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    let ihl = buf.first().map_or(0, |b| usize::from(b & 0x0f) * 4);
    if n < 20 || ihl < 20 || ihl > n || buf[0] >> 4 != 4 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(IcmpIn {
        start: ihl,
        len: n - ihl,
        ttl: buf[8],
        src: Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]),
    })
}

// The host clipboard for `#^` under `emu -g` (docs/graphics.md, Section
// 12.4): the general pasteboard's plain text (`NSPasteboardTypeString`,
// `public.utf8-plain-text`).
//
// Prog threads call these, not the main thread that runs winit's event
// loop. AppKit does not mark NSPasteboard as main-thread-only (objc2
// declares it `InteriorMutable`, not `MainThreadOnly`); it talks to the
// pasteboard server over IPC and is used from other threads in practice.
// That is what this relies on. Each call makes its own autorelease pool,
// since prog threads have none and AppKit autoreleases internally.

/// The pasteboard's text, at most `MAXSNARF` bytes (cut on a character
/// boundary); empty when it holds no text.
pub fn clipboard_get() -> Option<String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: generalPasteboard and stringForType: take and return
        // objects of the declared types; NSPasteboardTypeString is a
        // constant AppKit defines.
        let text =
            unsafe { NSPasteboard::generalPasteboard().stringForType(NSPasteboardTypeString) };
        let mut text = text.map(|s| s.to_string()).unwrap_or_default();
        let max = purg_core::devsnarf::MAXSNARF;
        if text.len() > max {
            text.truncate(text.floor_char_boundary(max));
        }
        Some(text)
    })
}

/// Replaces the pasteboard's contents with `text`; false if AppKit
/// refuses.
pub fn clipboard_put(text: &str) -> bool {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    objc2::rc::autoreleasepool(|_| {
        let s = NSString::from_str(text);
        // SAFETY: as in clipboard_get; clearContents must come before
        // writing to take ownership of the pasteboard.
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            pb.setString_forType(&s, NSPasteboardTypeString)
        }
    })
}

/// The registers at a fault, from the context macOS passes a signal
/// handler.
pub fn fault_registers(uc: *mut libc::c_void) -> super::unix::FaultRegisters {
    let mut regs = super::unix::FaultRegisters {
        pc: 0,
        sp: 0,
        fp: 0,
        lr: 0,
    };
    if uc.is_null() {
        return regs;
    }
    // SAFETY: an SA_SIGINFO handler's third argument is the ucontext_t
    // of the interrupted thread, whose machine context it points to.
    let mc = unsafe { (*uc.cast::<libc::ucontext_t>()).uc_mcontext };
    if mc.is_null() {
        return regs;
    }
    // SAFETY: mc is the non-null machine context of that ucontext_t.
    let ss = unsafe { &(*mc).__ss };
    #[cfg(target_arch = "aarch64")]
    {
        regs.pc = ss.__pc as usize;
        regs.sp = ss.__sp as usize;
        regs.fp = ss.__fp as usize;
        regs.lr = ss.__lr as usize;
    }
    #[cfg(target_arch = "x86_64")]
    {
        regs.pc = ss.__rip as usize;
        regs.sp = ss.__rsp as usize;
        regs.fp = ss.__rbp as usize;
    }
    regs
}

#[cfg(test)]
mod tests {
    use super::{clipboard_get, clipboard_put};

    /// A string goes onto the pasteboard and comes back. The pasteboard
    /// is the user's, so its text is put back afterwards (only the text:
    /// other types on it, such as an image, are lost). If the pasteboard
    /// refuses the write, as it might with no login session, the test
    /// says so and passes; GitHub's macOS runners have one.
    #[test]
    fn pasteboard_round_trip() {
        let before = clipboard_get().unwrap_or_default();
        let text = "purgatorio snarf test: αβγ ☃";
        if !clipboard_put(text) {
            eprintln!("skipped: the pasteboard refused a write");
            return;
        }
        let got = clipboard_get();
        clipboard_put(&before);
        assert_eq!(got.as_deref(), Some(text));
        // The 1 MiB limit holds on the way in.
        let big = "é".repeat(purg_core::devsnarf::MAXSNARF / 2 + 1);
        if clipboard_put(&big) {
            let got = clipboard_get().unwrap_or_default();
            clipboard_put(&before);
            assert!(got.len() <= purg_core::devsnarf::MAXSNARF);
            assert!(big.starts_with(&got));
        }
    }
}
