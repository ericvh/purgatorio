//! macOS-specific host code.

use std::ffi::CString;
use std::io;
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
