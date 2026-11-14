//! POSIX-common host code, shared by Linux and macOS.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use purg_abi::error;

/// Maps a host I/O error to an Inferno error string.
pub fn error_string(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => error::ENONEXIST.into(),
        io::ErrorKind::PermissionDenied => error::EPERM.into(),
        io::ErrorKind::NotADirectory => error::ENOTDIR.into(),
        io::ErrorKind::IsADirectory => error::EISDIR.into(),
        io::ErrorKind::AlreadyExists => error::EEXIST.into(),
        io::ErrorKind::Interrupted => error::EINTR.into(),
        _ => {
            let s = e.to_string();
            // Drop std's " (os error N)" suffix; Inferno strings are plain.
            match s.find(" (os error") {
                Some(i) => s[..i].to_lowercase(),
                None => s.to_lowercase(),
            }
        }
    }
}

/// Reads at an absolute offset without moving the file position.
pub fn read_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    f.read_at(buf, offset)
}

/// Writes at an absolute offset without moving the file position.
pub fn write_at(f: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    f.write_at(buf, offset)
}

/// Host identity of a file, used to build its Qid.
pub struct FileId {
    pub path: u64,
    pub vers: u32,
}

/// `qid.path` mixes `st_dev` and `st_ino`; `qid.vers` changes when the file's
/// mtime or size does.
pub fn file_id(md: &Metadata) -> FileId {
    let path = md.ino() ^ md.dev().rotate_left(48);
    let vers = (md.mtime_nsec() as u64 ^ md.mtime() as u64 ^ md.size()) as u32;
    FileId { path, vers }
}

fn user_by_uid(uid: u32) -> Option<String> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; `buf` outlives the
    // borrowed `pw_name`, which is copied before returning.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_name is a NUL-terminated string in `buf`.
    let name = unsafe { CStr::from_ptr(pw.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn group_by_gid(gid: u32) -> Option<String> {
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as for getpwuid_r above.
    let rc = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getgrgid_r succeeded, so gr_name is a NUL-terminated string in `buf`.
    let name = unsafe { CStr::from_ptr(gr.gr_name) };
    Some(name.to_string_lossy().into_owned())
}

fn cached(
    cache: &Mutex<HashMap<u32, String>>,
    id: u32,
    lookup: fn(u32) -> Option<String>,
) -> String {
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    c.entry(id)
        .or_insert_with(|| lookup(id).unwrap_or_else(|| id.to_string()))
        .clone()
}

/// Name of the user owning a file, as shown in `Dir.uid`.
pub fn owner_name(uid: u32) -> String {
    static USERS: std::sync::OnceLock<Mutex<HashMap<u32, String>>> = std::sync::OnceLock::new();
    cached(USERS.get_or_init(Default::default), uid, user_by_uid)
}

/// Name of a file's group, as shown in `Dir.gid`.
pub fn group_name(gid: u32) -> String {
    static GROUPS: std::sync::OnceLock<Mutex<HashMap<u32, String>>> = std::sync::OnceLock::new();
    cached(GROUPS.get_or_init(Default::default), gid, group_by_gid)
}

/// The invoking user's name, for `#c/user`.
pub fn user_name() -> String {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    user_by_uid(uid)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| String::from("none"))
}

/// The host name, for `#c/sysname`.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for writes of buf.len() bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::from("purgatorio");
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Puts the terminal on stdin into or out of raw mode. Not a terminal is a
/// no-op, so pipes and the test harness are unaffected.
pub fn set_console_raw(on: bool) -> Result<(), String> {
    let fd = libc::STDIN_FILENO;
    // SAFETY: isatty only reads the fd.
    if unsafe { libc::isatty(fd) } == 0 {
        return Ok(());
    }
    static ORIG: Mutex<Option<libc::termios>> = Mutex::new(None);
    let mut saved = ORIG.lock().unwrap_or_else(|e| e.into_inner());
    if on {
        let mut term = unsafe { core::mem::zeroed::<libc::termios>() };
        // SAFETY: term is a valid termios out-parameter.
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return Err(error::EIO.into());
        }
        if saved.is_none() {
            *saved = Some(term);
        }
        // SAFETY: cfmakeraw only writes the struct.
        unsafe { libc::cfmakeraw(&mut term) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
            return Err(error::EIO.into());
        }
    } else if let Some(term) = saved.take() {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    Ok(())
}

/// Fills `buf` from the OS entropy source.
pub fn entropy(buf: &mut [u8]) {
    // getentropy is limited to 256 bytes per call on both Linux and macOS.
    for chunk in buf.chunks_mut(256) {
        // SAFETY: chunk is valid for writes of chunk.len() <= 256 bytes.
        let rc = unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
        assert_eq!(rc, 0, "getentropy failed");
    }
}

/// Ends the process with `code`, without running the C library's exit
/// handlers and library destructors.
///
/// With `-x` or a closed window the emu exits while progs' threads still
/// run, and `exit` is not safe then. libgcc's destructor frees its
/// table of registered unwind frames; a prog thread that is dropping a
/// compiled module at that moment calls `__deregister_frame`, finds the
/// table gone, and aborts the process (issue #145). Nothing the emu owns
/// needs an exit handler: standard output and error are flushed here.
pub fn exit(code: u8) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: _exit only ends the process.
    unsafe { libc::_exit(i32::from(code)) }
}

/// Makes an abort say where it happened. A Rust panic is reported by the
/// panic hook, but `abort` called from C (libgcc, the allocator) kills
/// the process with SIGABRT and often not a word; this prints the thread
/// and its prog first, then lets the abort go on to the default action
/// (and the core dump).
pub fn report_aborts() {
    // SAFETY: the action is fully initialised, and the handler only
    // makes async-signal-safe calls.
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = on_abort as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESETHAND;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGABRT, &sa, core::ptr::null_mut());
    }
}

/// The SIGABRT handler: one `write` of a line built on the stack.
extern "C" fn on_abort(sig: libc::c_int) {
    Line::new().put(b"emu: abort (SIGABRT) on ").whose().write();
    // SAFETY: the default action is back in place (SA_RESETHAND), so the
    // raise ends the process when this handler returns, whoever sent the
    // signal.
    unsafe { libc::raise(sig) };
}

/// A line of a signal report, built on the stack: a signal handler may
/// not allocate or lock, since the signal may have come from inside the
/// allocator.
struct Line {
    buf: [u8; 256],
    n: usize,
}

impl Line {
    fn new() -> Line {
        Line {
            buf: [0; 256],
            n: 0,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> &mut Line {
        for &b in bytes {
            if self.n < self.buf.len() {
                self.buf[self.n] = b;
                self.n += 1;
            }
        }
        self
    }

    fn dec(&mut self, v: i64) -> &mut Line {
        if v < 0 {
            self.put(b"-");
        }
        let mut v = v.unsigned_abs();
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.put(&digits[at..])
    }

    fn hex(&mut self, mut v: usize) -> &mut Line {
        let mut digits = [0u8; 16];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b"0123456789abcdef"[v & 15];
            v >>= 4;
            if v == 0 {
                break;
            }
        }
        self.put(b"0x").put(&digits[at..])
    }

    /// A code address as `0x... (emu+0x...)`: the offset in the emu's
    /// image is what `addr2line -e emu` (or `atos`) takes.
    fn code(&mut self, pc: usize) -> &mut Line {
        self.hex(pc);
        let base = IMAGE_BASE.load(Ordering::Relaxed);
        // Only an address in the emu's image, which is well under 4 GiB.
        if base != 0 && pc >= base && pc - base < 1 << 32 {
            self.put(b" (emu+").hex(pc - base).put(b")");
        }
        self
    }

    /// The calling thread and its prog: `thread 'name', prog N`.
    fn whose(&mut self) -> &mut Line {
        let mut name = [0u8; 64];
        // SAFETY: name is valid for writes of its length, and the call
        // NUL-terminates what it writes.
        let named = unsafe {
            libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len())
        };
        let len = name.iter().position(|&b| b == 0).unwrap_or(0);
        self.put(b"thread '").put(if named == 0 && len > 0 {
            &name[..len]
        } else {
            b"?"
        });
        self.put(b"', prog ")
            .dec(i64::from(crate::platform::current_pid()))
    }

    /// Writes the line and a newline to standard error, in one `write`.
    fn write(&mut self) {
        self.put(b"\n");
        // SAFETY: buf is valid for reads of n bytes.
        unsafe { libc::write(libc::STDERR_FILENO, self.buf.as_ptr().cast(), self.n) };
    }
}

/// Where the emu's image is loaded, for the offsets in a fault report.
static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);

/// The actions SIGSEGV and SIGBUS had before [`report_faults`]: std's
/// handler, which reports a stack overflow on a thread's guard page.
static PREVIOUS: OnceLock<[libc::sigaction; 2]> = OnceLock::new();

/// A pipe through which the fault handler reads memory that may not be
/// mapped: `write` fails with EFAULT where a load would fault again.
static PROBE: OnceLock<[libc::c_int; 2]> = OnceLock::new();

/// The most return addresses a fault report shows, and the most frames
/// it follows (a deep recursion is shown as one address and a count).
const FRAMES: usize = 32;
const STEPS: usize = 1 << 16;

/// Makes a memory fault (SIGSEGV, SIGBUS) say where it happened: an emu
/// once died of SIGSEGV with nothing on its standard error (issue #239).
/// The handler prints the signal, the faulting address, the thread and
/// its prog, the program counter and other registers, and the return
/// addresses found by following frame pointers, where the code keeps
/// them. Then it puts the previous action back and returns, so the
/// faulting instruction runs again into std's handler: that one says
/// "has overflowed its stack" for a guard page, and otherwise lets the
/// default action end the process with a core dump. Nothing is spent
/// until a fault.
///
/// Call it from `main`, after std has installed its handler and before
/// other threads start.
pub fn report_faults() {
    let here = report_faults as fn() as usize;
    // SAFETY: dladdr only reads the loader's tables and fills `info`.
    unsafe {
        let mut info: libc::Dl_info = core::mem::zeroed();
        if libc::dladdr(here as *const libc::c_void, &mut info) != 0 {
            IMAGE_BASE.store(info.dli_fbase as usize, Ordering::Relaxed);
        }
    }
    let mut fds = [-1; 2];
    // SAFETY: fds has room for the two descriptors pipe returns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
        for fd in fds {
            // SAFETY: fd is one of the pipe's descriptors, just made.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
            }
        }
        let _ = PROBE.set(fds);
    }
    // SAFETY: zeroed sigactions are valid, and the handler only makes
    // async-signal-safe calls. SA_ONSTACK runs it on the thread's
    // alternate signal stack, which std gives the threads it starts, so
    // that a stack overflow still gets as far as std's handler.
    unsafe {
        let mut old: [libc::sigaction; 2] = core::mem::zeroed();
        for (sig, old) in [libc::SIGSEGV, libc::SIGBUS].into_iter().zip(&mut old) {
            libc::sigaction(sig, core::ptr::null(), old);
        }
        if PREVIOUS.set(old).is_err() {
            return;
        }
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = on_fault
            as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
            as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGSEGV, libc::SIGBUS] {
            libc::sigaction(sig, &sa, core::ptr::null_mut());
        }
    }
}

/// The two words at `at`, if they are mapped, read through [`PROBE`].
fn probe_pair(at: usize) -> Option<[usize; 2]> {
    let [rd, wr] = *PROBE.get()?;
    let mut out = [0usize; 2];
    let len = core::mem::size_of_val(&out);
    // SAFETY: the kernel reads the `len` bytes at `at` for write, which
    // fails with EFAULT, rather than faulting, where they are not
    // mapped; the read fills `out`, which has room for them.
    unsafe {
        if libc::write(wr, at as *const libc::c_void, len) != len as isize {
            return None;
        }
        if libc::read(rd, out.as_mut_ptr().cast(), len) != len as isize {
            return None;
        }
    }
    Some(out)
}

/// The SIGSEGV and SIGBUS handler (see [`report_faults`]).
extern "C" fn on_fault(sig: libc::c_int, info: *mut libc::siginfo_t, uc: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo to an SA_SIGINFO handler.
    let (addr, code) = unsafe { ((*info).si_addr() as usize, (*info).si_code) };
    let regs = super::os::fault_registers(uc);
    let what: &[u8] = if sig == libc::SIGBUS {
        b"emu: bus error (SIGBUS) at "
    } else {
        b"emu: memory fault (SIGSEGV) at "
    };
    Line::new()
        .put(what)
        .hex(addr)
        .put(b" on ")
        .whose()
        .put(b", pc ")
        .code(regs.pc)
        .write();
    Line::new()
        .put(b"emu:   lr ")
        .code(regs.lr)
        .put(b" sp ")
        .hex(regs.sp)
        .put(b" fp ")
        .hex(regs.fp)
        .put(b" si_code ")
        .dec(i64::from(code))
        .write();
    // The return address saved beside each frame pointer, up the stack:
    // the AArch64 and x86-64 frame records are both the caller's frame
    // pointer and then the return address. A run of one return address
    // (a recursion) is one line and a count. Two threads faulting at once
    // share the probe pipe and may mix up each other's frames.
    let mut fp = regs.fp;
    let (mut last, mut again) = (0, 0);
    let mut shown = 0;
    for _ in 0..STEPS {
        if shown == FRAMES {
            break;
        }
        if fp == 0 || !fp.is_multiple_of(core::mem::size_of::<usize>()) || fp < regs.sp {
            break;
        }
        let Some([next, ret]) = probe_pair(fp) else {
            break;
        };
        if ret == 0 {
            break;
        }
        if ret == last {
            again += 1;
        } else {
            if again > 0 {
                Line::new()
                    .put(b"emu:   ... ")
                    .dec(again)
                    .put(b" more of it")
                    .write();
            }
            Line::new().put(b"emu:   called from ").code(ret).write();
            (last, again) = (ret, 0);
            shown += 1;
        }
        if next <= fp {
            break;
        }
        fp = next;
    }
    if again > 0 {
        Line::new()
            .put(b"emu:   ... ")
            .dec(again)
            .put(b" more of it")
            .write();
    }
    // SAFETY: PREVIOUS holds the actions report_faults read; signal,
    // sigaction and raise are async-signal-safe.
    unsafe {
        if super::os::sent_by_kill(code) {
            // A signal sent by kill does not come again when the handler
            // returns, as a fault does, and std's handler would ignore it
            // (it only puts the default action back): send it again, to
            // the default action, which acts on it once this returns.
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        } else if let Some(old) = PREVIOUS.get() {
            libc::sigaction(
                sig,
                &old[usize::from(sig == libc::SIGBUS)],
                core::ptr::null_mut(),
            );
        }
    }
}

/// Registers at a fault, from the signal context, for [`on_fault`]. Zero
/// where the CPU has no such register (`lr` on x86-64).
pub struct FaultRegisters {
    pub pc: usize,
    pub sp: usize,
    pub fp: usize,
    pub lr: usize,
}
