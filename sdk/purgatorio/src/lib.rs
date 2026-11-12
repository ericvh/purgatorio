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

pub mod arg;
pub mod auth;
pub mod bufio;
pub mod daytime;
pub mod dial;
pub mod env;
pub mod file2chan;
pub mod ns;
pub mod styx;
pub mod sys;
pub mod task;

use std::fmt;
use std::os::fd::{FromRawFd, OwnedFd};

pub use purg_abi as abi;
pub use purg_styx::{DMAPPEND, DMAUTH, DMDIR, DMEXCL, Dir, Qid};
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

fn fd(n: i32) -> Result<OwnedFd, Error> {
    check(n)?;
    // SAFETY: the host just allocated fd `n` for this prog, and nothing
    // else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

/// `sys->open` with an Inferno mode (`abi::mode`). The fd closes on drop;
/// `std::fs::File::from` turns it into a `File`.
pub fn open(path: &str, mode: u8) -> Result<OwnedFd, Error> {
    fd(unsafe { sys::open(path.as_ptr(), path.len(), mode.into()) })
}

/// `sys->create`: makes `path` with Inferno permission bits `perm`
/// (`DMDIR` for a directory), truncating an existing file, and opens it in
/// `mode`.
pub fn create(path: &str, mode: u8, perm: u32) -> Result<OwnedFd, Error> {
    fd(unsafe { sys::create(path.as_ptr(), path.len(), mode.into(), perm) })
}

/// `sys->remove`.
pub fn remove(path: &str) -> Result<(), Error> {
    check(unsafe { sys::remove(path.as_ptr(), path.len()) }).map(drop)
}

/// `sys->wstat`: applies the fields of `d` that are not "don't touch"
/// (start from [`Dir::null`]).
pub fn wstat(path: &str, d: &Dir) -> Result<(), Error> {
    let buf = d.pack().map_err(|e| Error(e.to_string()))?;
    check(unsafe { sys::wstat(path.as_ptr(), path.len(), buf.as_ptr(), buf.len()) }).map(drop)
}

/// `sys->fwstat` on a raw fd.
pub fn fwstat(fd: i32, d: &Dir) -> Result<(), Error> {
    let buf = d.pack().map_err(|e| Error(e.to_string()))?;
    check(unsafe { sys::fwstat(fd, buf.as_ptr(), buf.len()) }).map(drop)
}

/// `sys->fd2path`: the name an fd was opened by.
pub fn fd2path(fd: i32) -> Result<String, Error> {
    let mut buf = vec![0u8; 8192];
    let n = check(unsafe { sys::fd2path(fd, buf.as_mut_ptr(), buf.len()) })?;
    buf.truncate(n);
    String::from_utf8(buf).map_err(|e| Error(e.to_string()))
}

/// The current directory.
pub fn dot() -> Result<String, Error> {
    fd2path(abi::PREOPEN_DOT_FD)
}

/// `sys->chdir`. Also moves the current directory that wasi-libc keeps for
/// relative names in `std`.
pub fn chdir(path: &str) -> Result<(), Error> {
    check(unsafe { sys::chdir(path.as_ptr(), path.len()) })?;
    sync_cwd();
    Ok(())
}

/// wasi-libc resolves relative names against its own copy of the current
/// directory, which starts as `/`; point it at the prog's dot.
fn sync_cwd() {
    if let Ok(d) = dot() {
        let _ = std::env::set_current_dir(d);
    }
}

/// `sys->dup`: copies `old` to `new`, or to the lowest free fd when `new` is
/// -1. Returns the new fd, which the caller owns.
pub fn dup(old: i32, new: i32) -> Result<i32, Error> {
    check(unsafe { sys::dup(old, new) }).map(|n| n as i32)
}

/// `sys->pipe`: two connected fds; data written on one is read from the
/// other.
pub fn pipe() -> Result<[OwnedFd; 2], Error> {
    let mut fds = [-1i32; 2];
    check(unsafe { sys::pipe(fds.as_mut_ptr()) })?;
    Ok([fd(fds[0])?, fd(fds[1])?])
}

/// Instantiates the reactor at `path` in this prog. The handle is passed to
/// [`call`].
pub fn load(path: &str) -> Result<i32, Error> {
    let n = check(unsafe { sys::load(path.as_ptr(), path.len()) })?;
    i32::try_from(n).map_err(|_| Error::from("bad module handle"))
}

/// Calls export `name` in a module from [`load`]. `args` are the raw bytes
/// the export reads; the bytes it writes back are the result.
pub fn call(handle: i32, name: &str, args: &[u8]) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; 65536];
    let n = check(unsafe {
        sys::call(
            handle,
            name.as_ptr(),
            name.len(),
            args.as_ptr(),
            args.len(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    })?;
    buf.truncate(n);
    Ok(buf)
}

/// `sys->iounit`: the largest atomic I/O on `fd`; 0 means no limit.
pub fn iounit(fd: i32) -> Result<usize, Error> {
    check(unsafe { sys::iounit(fd) })
}

/// `sys->bind` with flags from `abi::mount`.
pub fn bind(name: &str, old: &str, flags: u32) -> Result<(), Error> {
    check(unsafe { sys::bind(name.as_ptr(), name.len(), old.as_ptr(), old.len(), flags) }).map(drop)
}

/// `sys->mount`. `afd` is -1, so the mount authenticates by itself (or not
/// at all with `abi::mount::MNOAUTH`), or an fd from [`fauth`] on which the
/// caller has already authenticated (see [`auth`]). `aname` is the attach
/// name, often empty.
pub fn mount(fd: i32, afd: i32, old: &str, flags: u32, aname: &str) -> Result<(), Error> {
    check(unsafe {
        sys::mount(
            fd,
            afd,
            old.as_ptr(),
            old.len(),
            flags,
            aname.as_ptr(),
            aname.len(),
        )
    })
    .map(drop)
}

/// `sys->fauth`: starts authentication on the 9P conversation on `fd` for
/// attach name `aname`, returning the auth fd to pass to [`mount`]. A server
/// that does not authenticate fails with
/// [`abi::auth::NOT_REQUIRED`]; [`auth::amount`] handles that case.
pub fn fauth(fd: i32, aname: &str) -> Result<OwnedFd, Error> {
    self::fd(unsafe { sys::fauth(fd, aname.as_ptr(), aname.len()) })
}

/// `sys->stream`: copies `src` to `dst` in the kernel until end of file, an
/// error or a kill, through a `bufsiz` buffer. Returns the bytes written.
/// When the copy stopped on an error, the count is still returned and
/// [`errstr`] says why.
pub fn stream(src: i32, dst: i32, bufsiz: usize) -> Result<usize, Error> {
    let bufsiz = i32::try_from(bufsiz).unwrap_or(i32::MAX);
    check(unsafe { sys::stream(src, dst, bufsiz) })
}

/// `sys->export`: serve `dir` on `fd` until the client closes the connection.
pub fn export(fd: i32, dir: &str, flags: u32) -> Result<(), Error> {
    check(unsafe { sys::export(fd, dir.as_ptr(), dir.len(), flags) }).map(drop)
}

/// [`export`] whose server proof is signed with the authinfo `keyfile` (a
/// path) instead of this user's default keys: `export -k`.
pub fn export_key(fd: i32, dir: &str, flags: u32, keyfile: &str) -> Result<(), Error> {
    check(unsafe {
        sys::export_key(
            fd,
            dir.as_ptr(),
            dir.len(),
            flags,
            keyfile.as_ptr(),
            keyfile.len(),
        )
    })
    .map(drop)
}

/// `sys->unmount`: removes `name` from the union at `old`, or everything
/// mounted there when `name` is `None`.
pub fn unmount(name: Option<&str>, old: &str) -> Result<(), Error> {
    let name = name.unwrap_or("");
    check(unsafe { sys::unmount(name.as_ptr(), name.len(), old.as_ptr(), old.len()) }).map(drop)
}

/// `sys->pctl` with flags from `abi::pctl`; `movefd` lists the fds `NEWFD`
/// keeps. Returns the prog's pid.
pub fn pctl(flags: u32, movefd: &[i32]) -> Result<u32, Error> {
    let n = check(unsafe { sys::pctl(flags, movefd.as_ptr(), movefd.len()) })?;
    if flags & (abi::pctl::NEWNS | abi::pctl::FORKNS) != 0 {
        sync_cwd();
    }
    Ok(n as u32)
}

/// This prog's pid.
pub fn pid() -> u32 {
    pctl(0, &[]).unwrap_or(0)
}

/// Finds the module a command name runs, as sh does. A name that starts
/// with `/`, `./`, `../` or `#` is a path, tried as given and then with
/// `.wasm` added; any other name, `ls` or `disk/chkdisk`, is
/// `/bin/<name>.wasm`. A directory does not count. The error is the last
/// one seen.
pub fn find_command(name: &str) -> Result<String, Error> {
    let candidates =
        if name.starts_with(['/', '#']) || name.starts_with("./") || name.starts_with("../") {
            vec![String::from(name), format!("{name}.wasm")]
        } else {
            vec![format!("/bin/{name}.wasm")]
        };
    let mut err = Error::from(abi::error::ENONEXIST);
    for c in candidates {
        match stat(&c) {
            Ok(d) if !d.qid.is_dir() => return Ok(c),
            Ok(_) => err = Error::from(abi::error::EISDIR),
            Err(e) => err = e,
        }
    }
    Err(err)
}

/// Starts `cmd` (resolved like a shell command: `cat` is `/bin/cat.wasm`)
/// as a new prog with arguments `argv`, and returns its pid. The child
/// shares this prog's namespace, fds and environment except as `flags`
/// (from `abi::pctl`) say; `fdmap` pairs give it copies of this prog's fds,
/// as `(fd here, fd in the child)`, and imply `FORKFD`.
pub fn spawn(cmd: &str, argv: &[String], flags: u32, fdmap: &[(i32, i32)]) -> Result<u32, Error> {
    let mut packed = Vec::new();
    for a in argv {
        packed.extend_from_slice(a.as_bytes());
        packed.push(0);
    }
    let map: Vec<i32> = fdmap.iter().flat_map(|&(a, b)| [a, b]).collect();
    let n = check(unsafe {
        sys::spawn_prog(
            cmd.as_ptr(),
            cmd.len(),
            packed.as_ptr(),
            packed.len(),
            flags,
            map.as_ptr(),
            fdmap.len(),
        )
    })?;
    Ok(n as u32)
}

/// How a child prog ended, from `/prog/<pid>/wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waitmsg {
    pub pid: u32,
    pub name: String,
    /// The exit status: empty for success.
    pub status: String,
}

/// Blocks until a child of this prog exits and reports it.
pub fn wait() -> Result<Waitmsg, Error> {
    let path = format!("/prog/{}/wait", pid());
    let f = open(&path, abi::mode::OREAD)?;
    let mut buf = vec![0u8; 8192];
    let n = std::io::Read::read(&mut std::fs::File::from(f), &mut buf)?;
    let msg = String::from_utf8_lossy(&buf[..n]);
    let fields = unquote_fields(&msg);
    match fields.as_slice() {
        [pid, name, status] => Ok(Waitmsg {
            pid: pid.parse().map_err(|_| Error::from("bad wait message"))?,
            name: name.clone(),
            status: status.clone(),
        }),
        _ => Err(Error::from("bad wait message")),
    }
}

/// Splits `s` at white space, honouring Inferno's `%q` quoting: a field in
/// `'` quotes may hold white space, and `''` inside quotes is one `'`.
pub fn unquote_fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return out;
        }
        let mut field = String::new();
        let mut quoted = false;
        while let Some(c) = chars.next() {
            if quoted {
                if c == '\'' {
                    if chars.next_if_eq(&'\'').is_some() {
                        field.push('\'');
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(c);
                }
            } else if c == '\'' {
                quoted = true;
            } else if c.is_whitespace() {
                break;
            } else {
                field.push(c);
            }
        }
        out.push(field);
    }
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
            // `print!`, `println!` and `eprint!` panic when the write fails,
            // as when a pipe's reader has gone (`grep -s` stops at the first
            // match). In Inferno `sys->print` returns -1 there and the
            // program carries on to its normal end, printing nothing about
            // it; here the program has no way to carry on from inside the
            // macro, so it ends quietly, as it would have.
            if msg.starts_with("failed printing to std") {
                exits("");
            }
            eprintln!("panic: {msg}");
            werrstr(&msg);
            exits(&format!("panic: {msg}"));
        }));
        sync_cwd();
        let args: Vec<String> = std::env::args().collect();
        let argv0 = args.first().cloned().unwrap_or_default();
        if let Err(e) = main(Context, args) {
            eprintln!("{argv0}: {e}");
            exits(&format!("fail:{e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unquote_fields;

    #[test]
    fn unquote() {
        assert_eq!(unquote_fields("12 cat ''"), ["12", "cat", ""]);
        assert_eq!(
            unquote_fields("  7 'my prog' 'fail:it''s bad'\n"),
            ["7", "my prog", "fail:it's bad"]
        );
        assert!(unquote_fields(" ").is_empty());
    }
}
