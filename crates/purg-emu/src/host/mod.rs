//! Host operating system interface. This module is the only place in the emu
//! allowed to depend on the target OS: POSIX-common code is in `unix`, and
//! `linux`/`macos` hold the differences.

mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("purg-emu supports Linux and macOS hosts");

pub use os::{exact_name_exists, hidden_entry, memory_usage};
pub use unix::{
    entropy, error_string, file_id, group_name, host_name, owner_name, read_at, set_console_raw,
    user_name, write_at,
};
