//! import [-AS] [-C alg] [-k keyfile] [-9 tree] addr mtpt: dial `addr` and mount the
//! 9P server there. The mount authenticates with `fauth` when the server
//! asks, with the authinfo `keyfile` if given; `-A` skips authentication.
//! `-S` requires the server to prove itself too, as `mount -S` does: with
//! a key or certificate trusted for the host `addr` names. `-C alg` picks
//! the key exchange as `mount -C` does (`x25519mlkem768` requires the
//! post-quantum one).
//!
//! `-9 tree` (an addition) is for a Plan 9 `exportfs`, or `9export` here:
//! before 9P, the name of the tree wanted is sent and the server's `OK`
//! awaited, as Plan 9's `import` does. Plan 9's own authentication is not
//! implemented, so this implies `-A` and the server must not ask for it.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::mount::{MNOAUTH, MPQ, MREPL, MSRVAUTH};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

const USAGE: &str = "import [-AS] [-C alg] [-k keyfile] [-9 tree] addr mtpt";

/// `exportfs`'s opening exchange on `fd`.
fn plan9(fd: i32, tree: &str) -> Result<(), Error> {
    // SAFETY: the connection stays open in the caller; ManuallyDrop keeps
    // this File from closing it.
    let mut conn = std::mem::ManuallyDrop::new(unsafe {
        <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd)
    });
    conn.write_all(tree.as_bytes())?;
    let mut buf = [0u8; 128];
    let n = conn.read(&mut buf)?;
    if &buf[..n] != b"OK" {
        return Err(Error(format!(
            "bad remote tree: {}",
            String::from_utf8_lossy(&buf[..n])
        )));
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut flags = MREPL;
    let mut keyfile = None;
    let mut tree = None;
    while let Some(c) = arg.opt() {
        match c {
            'A' => flags |= MNOAUTH,
            'S' => flags |= MSRVAUTH,
            'k' => keyfile = Some(arg.earg()?),
            'C' => flags |= purgatorio::auth::kex_flags(&arg.earg()?)?,
            '9' => {
                tree = Some(arg.earg()?);
                flags |= MNOAUTH;
            }
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() != 2 || (flags & MNOAUTH != 0 && flags & (MSRVAUTH | MPQ) != 0) {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let conn = dial::dial(&rest[0])?;
    let fd = conn
        .data
        .as_ref()
        .ok_or_else(|| Error::from("no connection"))?
        .as_raw_fd();
    if let Some(tree) = tree {
        plan9(fd, &tree)?;
    }
    let host = dial::host(&rest[0]);
    purgatorio::auth::amount_host(fd, &rest[1], flags, "", keyfile.as_deref(), host.as_deref())?;
    drop(conn);
    Ok(())
}
