//! import [-AS] [-C alg] [-k keyfile] [-9 tree [-T authsrv]] addr mtpt: dial `addr` and mount the
//! 9P server there. The mount authenticates with `fauth` when the server
//! asks, with the authinfo `keyfile` if given; `-A` skips authentication.
//! `-S` requires the server to prove itself too, as `mount -S` does: with
//! a key or certificate trusted for the host `addr` names. `-C alg` picks
//! the key exchange and the signatures as `mount -C` does
//! (by default the hybrid post-quantum exchange when the server offers
//! it; `x25519mlkem768` requires it, `x25519` asks for X25519 alone,
//! `mldsa65` requires hybrid signatures from the server).
//!
//! `-9 tree` (an addition) is for a Plan 9 `exportfs`, or `9export` here:
//! the connection is authenticated with Plan 9's `p9any` (`p9sk1`), then
//! the name of the tree wanted is sent and the server's `OK` awaited, as
//! Plan 9's `import` does; the 9P that follows is not authenticated
//! again. `keyfile` holds the p9sk1 keys (factotum key lines, `key
//! proto=p9sk1 dom=<dom> user=<user> !password=<password>`). The tickets
//! come from the auth server `-T authsrv` (a dial string; a bare host is
//! its ticket port, 567), or, with none, are made locally when the key is
//! the server's own. `-A` with `-9` skips `p9any`, for a server that does
//! not authenticate (`9export -A`).

use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::mount::{MNOAUTH, MPQ, MPQSIG, MREPL, MSRVAUTH};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

const USAGE: &str = "import [-AS] [-C alg] [-k keyfile] [-9 tree [-T authsrv]] addr mtpt";

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
    let mut alg = None;
    let mut authsrv = None;
    while let Some(c) = arg.opt() {
        match c {
            'A' => flags |= MNOAUTH,
            'T' => authsrv = Some(arg.earg()?),
            'S' => flags |= MSRVAUTH,
            'k' => keyfile = Some(arg.earg()?),
            'C' => {
                let a = arg.earg()?;
                flags |= purgatorio::auth::kex_flags(&a)?;
                alg = Some(a);
            }
            '9' => tree = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let skip = flags & MNOAUTH != 0;
    if rest.len() != 2
        || (skip && flags & (MSRVAUTH | MPQ | MPQSIG) != 0)
        || (tree.is_some() && flags & (MSRVAUTH | MPQ | MPQSIG) != 0)
        || (authsrv.is_some() && (tree.is_none() || skip))
    {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let conn = dial::dial(&rest[0])?;
    let fd = conn
        .data
        .as_ref()
        .ok_or_else(|| Error::from("no connection"))?
        .as_raw_fd();
    if let Some(tree) = tree {
        if !skip {
            let keys = purgatorio::p9any::keys(
                keyfile
                    .as_deref()
                    .ok_or_else(|| Error::from("-9 needs -k keyfile with p9sk1 keys, or -A"))?,
            )?;
            // SAFETY: the connection stays open; ManuallyDrop keeps this
            // File from closing it.
            let mut conn = std::mem::ManuallyDrop::new(unsafe {
                <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd)
            });
            purgatorio::p9any::client(&mut *conn, &keys, authsrv.as_deref())?;
        }
        plan9(fd, &tree)?;
        // Plan 9's authentication was the connection's; 9P follows bare.
        flags |= MNOAUTH;
    }
    let host = dial::host(&rest[0]);
    let kx = purgatorio::auth::Kx::from_alg(alg.as_deref(), flags);
    purgatorio::auth::amount_kx(
        fd,
        &rest[1],
        flags,
        "",
        keyfile.as_deref(),
        host.as_deref(),
        kx,
    )?;
    drop(conn);
    Ok(())
}
