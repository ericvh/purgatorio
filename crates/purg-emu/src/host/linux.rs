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
