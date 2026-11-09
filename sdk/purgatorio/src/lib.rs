//! The Purgatorio application SDK.
//!
//! Apps are ordinary Rust programs for `wasm32-wasip1`: `std::fs`,
//! `std::io` and `std::env::args` work through the host's WASI layer, which
//! resolves names in the prog's Inferno namespace. This crate adds what WASI
//! cannot express: exact error strings ([`errstr`]), full [`Dir`]s, string
//! exit status ([`exits`]) and, in later milestones, bind, mount, pctl and
//! channels.
//!
//! ```ignore
//! #[purgatorio::main]
//! fn main(_ctx: purgatorio::Context, args: Vec<String>) -> Result<(), purgatorio::Error> {
//!     println!("{}", args[1..].join(" "));
//!     Ok(())
//! }
//! ```

pub mod sys;

use std::fmt;

pub use purg_abi as abi;
pub use purg_styx::{Dir, Qid};
pub use purgatorio_macros::main;

/// Length of [`ABI_BYTES`].
pub const ABI_LEN: usize = purg_abi::ABI_VERSION.len();

/// Contents of the `purgatorio.abi` custom section emitted by
/// [`macro@main`].
pub const ABI_BYTES: [u8; ABI_LEN] = {
    let src = purg_abi::ABI_VERSION.as_bytes();
    let mut out = [0u8; ABI_LEN];
    let mut i = 0;
    while i < ABI_LEN {
        out[i] = src[i];
        i += 1;
    }
    out
};

/// The context a command is started with. Limbo commands receive a
/// `ref Draw->Context`; graphics arrives in a late phase, so this is empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct Context;

/// An Inferno error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl Error {
    /// The prog's current errstr.
    pub fn last() -> Error {
        Error(errstr())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<&str> for Error {
    fn from(s: &str) -> Error {
        Error(s.into())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error(s)
    }
}

/// A failed std I/O call becomes the exact Inferno string the host recorded
/// for it; only errors that never reached the host keep std's text.
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        if e.raw_os_error().is_some() {
            let s = errstr();
            if !s.is_empty() {
                return Error(s);
            }
        }
        Error(e.to_string())
    }
}

fn check(n: i32) -> Result<usize, Error> {
    if n < 0 {
        Err(Error::last())
    } else {
        Ok(n as usize)
    }
}

/// The prog's error string: the exact text of the last failed system call.
pub fn errstr() -> String {
    let mut buf = vec![0u8; 256];
    let n = unsafe { sys::errstr(buf.as_mut_ptr(), buf.len()) };
    buf.truncate(n.max(0) as usize);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Sets the prog's error string.
pub fn werrstr(msg: &str) {
    unsafe { sys::werrstr(msg.as_ptr(), msg.len()) };
}

/// Exits the prog with an Inferno status string; `""` means success.
pub fn exits(status: &str) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    unsafe { sys::exits(status.as_ptr(), status.len()) }
}

fn dir_from(buf: &[u8], n: usize) -> Result<Dir, Error> {
    Dir::unpack(&buf[..n])
        .map(|(d, _)| d)
        .map_err(|e| Error(e.to_string()))
}

/// Size of a buffer that holds any packed Dir.
const MAXSTAT: usize = u16::MAX as usize + 2;

/// `sys->stat`: the full Inferno `Dir` of a file.
pub fn stat(path: &str) -> Result<Dir, Error> {
    let mut buf = vec![0u8; MAXSTAT];
    let n = check(unsafe { sys::stat(path.as_ptr(), path.len(), buf.as_mut_ptr(), buf.len()) })?;
    dir_from(&buf, n)
}

/// `sys->fstat` on a raw fd.
pub fn fstat(fd: i32) -> Result<Dir, Error> {
    let mut buf = vec![0u8; MAXSTAT];
    let n = check(unsafe { sys::fstat(fd, buf.as_mut_ptr(), buf.len()) })?;
    dir_from(&buf, n)
}

/// `sys->dirread` on a raw fd open on a directory: the next batch of
/// entries, empty at the end.
pub fn dirread(fd: i32) -> Result<Vec<Dir>, Error> {
    let mut buf = vec![0u8; 16 * 1024];
    let n = check(unsafe { sys::dirread(fd, buf.as_mut_ptr(), buf.len()) })?;
    purg_styx::dirs(&buf[..n])
        .collect::<Result<_, _>>()
        .map_err(|e| Error(e.to_string()))
}

/// Replaces the running command with `cmd` (resolved like a shell command:
/// `cat` is `/bin/cat.wasm`) in the same prog. Returns only on failure.
pub fn exec(cmd: &str, argv: &[String]) -> Error {
    let mut packed = Vec::new();
    for a in argv {
        packed.extend_from_slice(a.as_bytes());
        packed.push(0);
    }
    unsafe { sys::exec(cmd.as_ptr(), cmd.len(), packed.as_ptr(), packed.len()) };
    Error::last()
}

#[doc(hidden)]
pub mod rt {
    use super::*;

    /// Runs a `#[purgatorio::main]` function: installs the panic hook, calls
    /// it with argv, and turns an error into `argv0: msg` on fd 2 plus a
    /// `fail:` exit status.
    pub fn run(main: fn(Context, Vec<String>) -> Result<(), Error>) {
        std::panic::set_hook(Box::new(|info| {
            let msg = match info.payload().downcast_ref::<&str>() {
                Some(s) => (*s).to_string(),
                None => match info.payload().downcast_ref::<String>() {
                    Some(s) => s.clone(),
                    None => String::from("panic"),
                },
            };
            eprintln!("panic: {msg}");
            werrstr(&msg);
            exits(&format!("panic: {msg}"));
        }));
        let args: Vec<String> = std::env::args().collect();
        let argv0 = args.first().cloned().unwrap_or_default();
        if let Err(e) = main(Context, args) {
            eprintln!("{argv0}: {e}");
            exits(&format!("fail:{e}"));
        }
    }
}
