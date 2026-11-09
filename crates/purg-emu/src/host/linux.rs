//! Linux-specific host code.

use std::io;
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
