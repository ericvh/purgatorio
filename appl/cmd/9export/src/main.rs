//! 9export -A [-r]: serve part of this namespace on file descriptor 0 to
//! a Plan 9 `import`, which speaks `exportfs`'s opening protocol before
//! 9P: the client writes the name of the tree it wants in one message,
//! and the server answers `OK`, or the reason it cannot serve that tree.
//! 9P2000 follows. Start it from `listen`:
//!
//! ```text
//! listen -A 'tcp!*!17007' /bin/9export.wasm -A &
//! ```
//!
//! Plan 9 authenticates the connection with `p9any` first and may then
//! encrypt it (`import -e`). Neither exists here (`docs/auth.md`), so
//! `-A`, which says the connection is not authenticated, is required, and
//! only a client that also skips authentication can connect. `-r` serves
//! the tree read-only (an addition).

use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

use purgatorio::abi::{export, mount};
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};

const USAGE: &str = "9export -A [-r]";
/// The longest tree name `exportfs` reads.
const MAXTREE: usize = 255;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut noauth, mut flags) = (false, mount::MNOAUTH);
    while let Some(c) = arg.opt() {
        match c {
            'A' => noauth = true,
            'r' => flags |= export::RDONLY,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    if !noauth {
        return Err(Error::from(
            "Plan 9 authentication (p9any) is not implemented; use -A",
        ));
    }
    // SAFETY: fd 0 is the connection, open for this prog's whole life;
    // ManuallyDrop keeps the File from closing it before the export.
    let mut conn = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let mut buf = [0u8; MAXTREE];
    let n = conn.read(&mut buf)?;
    // The name may come with its terminating NUL.
    let name = buf[..n].split(|&b| b == 0).next().unwrap_or(&[]);
    let tree = String::from_utf8_lossy(name).into_owned();
    let refuse = |conn: &mut std::fs::File, why: String| {
        let _ = conn.write_all(why.as_bytes());
        Error(format!("{tree}: {why}"))
    };
    match purgatorio::stat(&tree) {
        Ok(d) if d.mode & DMDIR != 0 && tree.starts_with('/') => {}
        Ok(_) => return Err(refuse(&mut conn, String::from("not a directory"))),
        Err(e) => return Err(refuse(&mut conn, e.0)),
    }
    conn.write_all(b"OK")?;
    purgatorio::export(0, &tree, flags)
}
