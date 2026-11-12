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
