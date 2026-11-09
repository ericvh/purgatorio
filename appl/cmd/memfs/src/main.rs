//! memfs mtpt: mount a fresh in-memory tree at `mtpt` and serve it in the
//! background. `memfs --serve` is the server half, speaking 9P on fd 0.

use std::os::fd::{AsRawFd, FromRawFd};

use purgatorio::abi::mount::{MCREATE, MREPL};
use purgatorio::abi::pctl;
use purgatorio::styx::{self, Tree};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let io = unsafe { std::fs::File::from_raw_fd(0) };
        return styx::serve(io, &mut Tree::new());
    }
    let mtpt = args
        .get(1)
        .ok_or_else(|| Error::from("usage: memfs mtpt"))?;
    let [a, b] = purgatorio::pipe()?;
    purgatorio::spawn(
        "/bin/memfs.wasm",
        &[String::from("memfs"), String::from("--serve")],
        pctl::NEWFD,
        &[(a.as_raw_fd(), 0)],
    )?;
    drop(a);
    purgatorio::mount(b.as_raw_fd(), -1, mtpt, MREPL | MCREATE, "")?;
    drop(b);
    Ok(())
}
