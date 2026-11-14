//! Linux-specific host code.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

/// Linux file systems are case-sensitive and byte-exact, so a successful host
/// lookup already matched the exact name.
pub fn exact_name_exists(_dir: &Path, _name: &str) -> io::Result<bool> {
    Ok(true)
}

/// Host files hidden from `#U` directory listings.
pub fn hidden_entry(_name: &str) -> bool {
    false
}

/// The host clipboard for `#^`: none on Linux yet (X11's selections and
/// Wayland's data devices are left for later), so the device keeps its
/// own buffer.
pub fn clipboard_get() -> Option<String> {
    None
}

/// See [`clipboard_get`]: there is no host clipboard to put text on.
pub fn clipboard_put(_text: &str) -> bool {
    false
}

/// `(resident bytes, MemTotal bytes)` from procfs. Both are 0 when it cannot
/// be read.
pub fn memory_usage() -> (u64, u64) {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages.saturating_mul(page))
        .unwrap_or(0);
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let rest = l.strip_prefix("MemTotal:")?.trim();
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                Some(kb.saturating_mul(1024))
            })
        })
        .unwrap_or(0);
    (rss, total)
}

/// Why an ICMP socket could not be opened when the host refuses it.
pub const ICMP_DENIED: &str = "permission denied: this host does not allow unprivileged ICMP \
     sockets for the emu's group (see the sysctl net.ipv4.ping_group_range)";

/// Whether the host replaces an ICMP datagram socket's echo identifier
/// with its own and delivers only the replies that match it. Linux does.
pub const ICMP_IDENT_BY_HOST: bool = true;

/// An unprivileged ICMP datagram socket (`SOCK_DGRAM`, `IPPROTO_ICMP`),
/// which Linux allows to the groups in `net.ipv4.ping_group_range`. It
/// asks for each reply's TTL.
pub fn icmp_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the result is checked.
    let fd = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::IPPROTO_ICMP,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor this function owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let on: libc::c_int = 1;
    // SAFETY: the option value is a c_int of the size given.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_RECVTTL,
            (&on as *const libc::c_int).cast(),
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r < 0 {
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

/// Receives one message on an [`icmp_socket`]. Linux gives the ICMP
/// message alone; the TTL comes in a control message.
pub fn icmp_recv(fd: &impl AsRawFd, buf: &mut [u8]) -> io::Result<IcmpIn> {
    // SAFETY: zeroed sockaddr_in and msghdr are valid; every pointer set
    // below refers to a live local or to `buf` with its length.
    let mut name: libc::sockaddr_in = unsafe { core::mem::zeroed() };
    let mut control = [0u64; 16];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_name = (&mut name as *mut libc::sockaddr_in).cast();
    msg.msg_namelen = core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = core::mem::size_of_val(&control);
    // SAFETY: msg describes valid buffers, as above.
    let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut ttl = 0u8;
    // SAFETY: the CMSG macros walk the control buffer recvmsg filled in,
    // within msg_controllen; the TTL is read unaligned.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::IPPROTO_IP && (*c).cmsg_type == libc::IP_TTL {
                let v = core::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>());
                ttl = v.clamp(0, 255) as u8;
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok(IcmpIn {
        start: 0,
        len: n as usize,
        ttl,
        src: Ipv4Addr::from(u32::from_be(name.sin_addr.s_addr)),
    })
}

/// The registers at a fault, from the context Linux passes a signal
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
    // of the interrupted thread.
    let uc = unsafe { &*uc.cast::<libc::ucontext_t>() };
    #[cfg(target_arch = "aarch64")]
    {
        let m = &uc.uc_mcontext;
        regs.pc = m.pc as usize;
        regs.sp = m.sp as usize;
        regs.fp = m.regs[29] as usize;
        regs.lr = m.regs[30] as usize;
    }
    #[cfg(target_arch = "x86_64")]
    {
        let g = &uc.uc_mcontext.gregs;
        regs.pc = g[libc::REG_RIP as usize] as usize;
        regs.sp = g[libc::REG_RSP as usize] as usize;
        regs.fp = g[libc::REG_RBP as usize] as usize;
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = uc;
    regs
}

/// Whether a signal's `si_code` says a process sent it (`kill`,
/// `sigqueue`, `tgkill`) rather than the CPU raising it: Linux's codes
/// for those are 0 and below.
pub fn sent_by_kill(code: libc::c_int) -> bool {
    code <= 0
}
