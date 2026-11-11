//! mount [-abrcAS] [-k keyfile] fd-or-addr mtpt [spec]: mount a 9P
//! connection at `mtpt`. `fd-or-addr` is a file descriptor, a dial string
//! (`tcp!host!port`) or a `{command}` block, which is run by sh with a pipe
//! as its fds 0 and 1. The mount authenticates when the server asks, with
//! the authinfo `keyfile` if given (`docs/auth.md`); `-A` skips that. `-S`
//! (an addition) requires the server to prove itself as well, with a key
//! or certificate this namespace trusts as a server's
//! (`/lib/keys/servers`, `/lib/keys/signers`).

use std::os::fd::AsRawFd;

use purgatorio::abi::{mount, pctl};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

const USAGE: &str = "mount [-abrcAS] [-k keyfile] fd-or-addr mtpt [spec]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut order, mut create, mut noauth, mut srvauth) = (mount::MREPL, 0, 0, 0);
    let mut keyfile = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'r' => order = mount::MREPL,
            'c' => create = mount::MCREATE,
            'A' => noauth = mount::MNOAUTH,
            'S' => srvauth = mount::MSRVAUTH,
            'k' => keyfile = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() < 2 || rest.len() > 3 || (noauth != 0 && srvauth != 0) {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let spec = rest.get(2).map(String::as_str).unwrap_or("");
    // A dial string has a `!`. The connection has to stay open for the mount.
    let conn;
    let pipe;
    let fd = if let Some(cmd) = rest[0].strip_prefix('{').and_then(|c| c.strip_suffix('}')) {
        conn = None;
        let [a, b] = purgatorio::pipe().map_err(|e| Error(format!("mount: {e}")))?;
        purgatorio::spawn(
            "/bin/sh.wasm",
            &[String::from("sh"), String::from("-c"), String::from(cmd)],
            pctl::NEWFD,
            &[(a.as_raw_fd(), 0), (a.as_raw_fd(), 1), (2, 2)],
        )
        .map_err(|e| Error(format!("mount: {e}")))?;
        drop(a);
        pipe = Some(b);
        pipe.as_ref().map_or(-1, |b| b.as_raw_fd())
    } else if rest[0].contains('!') {
        pipe = None;
        conn = Some(dial::dial(&rest[0]).map_err(|e| Error(format!("mount: {e}")))?);
        conn.as_ref()
            .and_then(|c| c.data.as_ref())
            .map(|d| d.as_raw_fd())
            .ok_or_else(|| Error::from("mount: no connection"))?
    } else {
        conn = None;
        pipe = None;
        rest[0].parse().map_err(|_| Error::from("bad fd"))?
    };
    let flags = order | create | noauth | srvauth;
    // The kernel authenticates a plain mount itself, in one conversation.
    // A keyfile or a guest factotum means the proof is built here, with
    // fauth first.
    if keyfile.is_some() || purgatorio::auth::have_factotum() {
        purgatorio::auth::amount_key(fd, &rest[1], flags, spec, keyfile.as_deref())
    } else {
        purgatorio::mount(fd, -1, &rest[1], flags, spec)
    }
    .map_err(|e| Error(format!("mount: {e}")))?;
    // The mount's session holds the conversation. Dropping `conn` closes this
    // prog's descriptor and leaves the session's reference in place.
    drop(conn);
    drop(pipe);
    Ok(())
}
