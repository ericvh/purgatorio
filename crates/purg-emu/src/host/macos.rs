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
