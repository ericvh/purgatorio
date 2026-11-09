//! mount [-abrcA] fd-or-addr mtpt [spec]: mount a 9P connection at `mtpt`.
//! `fd-or-addr` is a file descriptor or a dial string (`tcp!host!port`).

use std::os::fd::AsRawFd;

use purgatorio::abi::mount;
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "mount [-abrcA] fd-or-addr mtpt [spec]");
    let (mut order, mut create) = (mount::MREPL, 0);
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'r' => order = mount::MREPL,
            'c' => create = mount::MCREATE,
            'A' => {}
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() < 2 || rest.len() > 3 {
        return Err(Error::from("usage: mount [-abrcA] fd-or-addr mtpt [spec]"));
    }
    let spec = rest.get(2).map(String::as_str).unwrap_or("");
    // A dial string has a `!`. The connection has to stay open for the mount.
    let conn;
    let fd = if rest[0].contains('!') {
        conn = Some(dial::dial(&rest[0]).map_err(|e| Error(format!("mount: {e}")))?);
        conn.as_ref()
            .and_then(|c| c.data.as_ref())
            .map(|d| d.as_raw_fd())
            .ok_or_else(|| Error::from("mount: no connection"))?
    } else {
        conn = None;
        rest[0].parse().map_err(|_| Error::from("bad fd"))?
    };
    purgatorio::mount(fd, -1, &rest[1], order | create, spec)
        .map_err(|e| Error(format!("mount: {e}")))?;
    // The mount's session holds the conversation. Dropping `conn` closes this
    // prog's descriptor and leaves the session's reference in place.
    drop(conn);
    Ok(())
}
