//! Running a file server in the background, as `memfs` does: the command
//! starts itself again with a pipe as fd 0 and mounts the other end.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};

use purgatorio::Error;
use purgatorio::abi::{mount, pctl};
use purgatorio::styx::{self, StyxFs};

/// The mount flags for the `-a`, `-b` and `-c` options file servers take,
/// or `None` for another letter.
pub fn flag(c: char, flags: &mut u32) -> Option<()> {
    match c {
        'a' => *flags = (*flags & !mount::MORDER) | mount::MAFTER,
        'b' => *flags = (*flags & !mount::MORDER) | mount::MBEFORE,
        'c' => *flags |= mount::MCREATE,
        _ => return None,
    }
    Some(())
}

/// Starts `module` with `argv`, a pipe as its fd 0 and this prog's fd 2 as
/// its fd 2 and a copy of this namespace (so a server of a directory
/// it then covers still sees what was there), then mounts the other end of the pipe on `mtpt` with `flags`.
/// The child serves the conversation with [`serve`].
pub fn mount(module: &str, argv: &[String], mtpt: &str, flags: u32) -> Result<(), Error> {
    let [a, b] = purgatorio::pipe()?;
    purgatorio::spawn(
        module,
        argv,
        pctl::NEWFD | pctl::FORKNS,
        &[(a.as_raw_fd(), 0), (2, 2)],
    )?;
    drop(a);
    purgatorio::mount(b.as_raw_fd(), -1, mtpt, flags | mount::MNOAUTH, "")?;
    Ok(())
}

/// Speaks 9P for `fs` on fd 0 until the client goes away.
pub fn serve(fs: &mut dyn StyxFs) -> Result<(), Error> {
    // SAFETY: fd 0 is the conversation [`mount`] gave this prog, and
    // nothing else in the prog uses it.
    let io = unsafe { File::from_raw_fd(0) };
    styx::serve(io, fs)
}
