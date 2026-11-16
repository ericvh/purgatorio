//! 9export [-A | -k keyfile] [-r]: serve part of this namespace on file
//! descriptor 0 to
//! a Plan 9 `import`, which speaks `exportfs`'s opening protocol before
//! 9P: the client writes the name of the tree it wants in one message,
//! and the server answers `OK`, or the reason it cannot serve that tree.
//! 9P2000 follows. Start it from `listen`:
//!
//! ```text
//! listen -A 'tcp!*!17007' /bin/9export.wasm -A &
//! ```
//!
//! Plan 9 authenticates the connection with `p9any` first. With `-k`,
//! 9export does that, as the server: it offers `p9sk1` in the domain of
//! the first p9sk1 key in `keyfile` (factotum key lines, `key
//! proto=p9sk1 dom=<dom> user=<user> !password=<password>`) and checks
//! the client's ticket, which the client gets from the domain's auth
//! server (`auth/authsrv`). The tree is then served as the user the
//! client authenticated as, when this prog may become that user (the
//! host owner may). `-A` skips authentication, for a client that skips it
//! too. Plan 9's encryption after authentication (`import -e`) is not
//! done. `-r` serves the tree read-only (an addition).

use purgatorio::fd::FromRawFd;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;

use purgatorio::abi::{export, mount};
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};

const USAGE: &str = "9export [-A | -k keyfile] [-r]";
/// The longest tree name `exportfs` reads.
const MAXTREE: usize = 255;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut noauth, mut flags, mut keyfile) = (false, mount::MNOAUTH, None);
    while let Some(c) = arg.opt() {
        match c {
            'A' => noauth = true,
            'k' => keyfile = Some(arg.earg()?),
            'r' => flags |= export::RDONLY,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() || noauth == keyfile.is_some() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    // SAFETY: fd 0 is the connection, open for this prog's whole life;
    // ManuallyDrop keeps the File from closing it before the export.
    let mut conn = ManuallyDrop::new(unsafe { purgatorio::fs::File::from_raw_fd(0) });
    if let Some(keyfile) = keyfile {
        let keys = purgatorio::p9any::keys(&keyfile)?;
        let user = purgatorio::p9any::server(&mut *conn, &keys[0])?;
        // Serve as the user the client proved, as Plan 9's exportfs does.
        let me = purgatorio::fs::read_to_string("/dev/user").unwrap_or_default();
        if me.trim() != user {
            purgatorio::fs::write("/dev/user", &user)
                .map_err(|e| Error(format!("cannot become {user}: {}", Error::from(e))))?;
        }
    }
    let mut buf = [0u8; MAXTREE];
    let n = conn.read(&mut buf)?;
    // The name may come with its terminating NUL.
    let name = buf[..n].split(|&b| b == 0).next().unwrap_or(&[]);
    let tree = String::from_utf8_lossy(name).into_owned();
    let refuse = |conn: &mut purgatorio::fs::File, why: String| {
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
