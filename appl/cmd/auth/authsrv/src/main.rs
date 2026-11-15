//! auth/authsrv [-k keyfile]: Plan 9's ticket service (authsrv(6)) for
//! p9sk1, on file descriptor 0, for `import -9`'s clients and any Plan 9
//! client of a Purgatorio domain. Start it from `listen` on the ticket
//! port:
//!
//! ```text
//! listen -A 'tcp!*!567' /bin/auth/authsrv.wasm -k /lib/keys/p9sk1 &
//! ```
//!
//! `keyfile` (default `/lib/keys/p9sk1`) holds the domain's users' keys
//! as factotum key lines (`key proto=p9sk1 dom=<dom> user=<user>
//! !password=<password>`). Each ticket request is answered with a ticket
//! for its host, sealed with the host's key, and one for its server,
//! sealed with the server's, or with the reason it is refused. A host
//! gets tickets for itself only: Plan 9's `speaksfor` is not kept. Only
//! ticket requests are served; Plan 9's password changes and challenge
//! boxes are not.

use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/authsrv [-k keyfile]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut keyfile = String::from("/lib/keys/p9sk1");
    while let Some(c) = arg.opt() {
        match c {
            'k' => keyfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let keys = purgatorio::p9any::keys(&keyfile)?;
    // SAFETY: fd 0 is the connection, open for this prog's whole life;
    // ManuallyDrop leaves it to the prog's exit.
    let mut conn = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    purgatorio::p9any::ticket_service(&mut *conn, &keys)
}
