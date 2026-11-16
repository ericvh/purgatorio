//! mount [-abrcAS] [-C alg] [-k keyfile] fd-or-addr mtpt [spec]: mount a 9P
//! connection at `mtpt`. `fd-or-addr` is a file descriptor, a dial string
//! (`tcp!host!port`) or a `{command}` block, which is run by sh with a pipe
//! as its fds 0 and 1. The mount authenticates when the server asks, with
//! the authinfo `keyfile` if given (`docs/auth.md`); `-A` skips that. `-S`
//! (an addition) requires the server to prove itself as well, with a key
//! or certificate this namespace trusts as a server's
//! (`/lib/keys/servers`, `/lib/keys/serversigners` or else
//! `/lib/keys/signers`), and for a dial string, one
//! for the host it names: a key listed for that host, or a certificate
//! with that name (`docs/auth.md`, "Servers per host"). `-C alg` picks
//! the key exchange, as Inferno's `-C` picks the algorithms pushed after
//! authentication. By default the mount uses the hybrid post-quantum
//! exchange (X25519 with ML-KEM-768) with a server that offers it, and
//! X25519 alone with a server that cannot (`docs/auth.md`, "The default
//! exchange"). `x25519mlkem768` requires the hybrid exchange: a server
//! that does not offer it fails with `no post-quantum key offered`.
//! `x25519` asks for X25519 alone. It also takes `mldsa65` (and `ed25519`, the default), for
//! hybrid Ed25519 + ML-DSA-65 signatures: the mount proves with a hybrid
//! key when factotum has one, and requires the server to prove itself
//! with a hybrid key or certificate, as `-S` does (`docs/auth.md`,
//! "Post-quantum signatures"). Several names are joined with `/`:
//! `-C x25519mlkem768/mldsa65`.

use purgatorio::fd::AsRawFd;

use purgatorio::abi::{mount, pctl};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

const USAGE: &str = "mount [-abrcAS] [-C alg] [-k keyfile] fd-or-addr mtpt [spec]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut order, mut create, mut noauth, mut srvauth) = (mount::MREPL, 0, 0, 0);
    let mut keyfile = None;
    let mut kex = 0;
    let mut alg = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'r' => order = mount::MREPL,
            'c' => create = mount::MCREATE,
            'A' => noauth = mount::MNOAUTH,
            'S' => srvauth = mount::MSRVAUTH,
            'k' => keyfile = Some(arg.earg()?),
            'C' => {
                let a = arg.earg()?;
                kex = purgatorio::auth::kex_flags(&a)?;
                alg = Some(a);
            }
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() < 2 || rest.len() > 3 || (noauth != 0 && (srvauth != 0 || kex != 0)) {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let spec = rest.get(2).map(String::as_str).unwrap_or("");
    // A dial string has a `!`. The connection has to stay open for the mount.
    let conn;
    let pipe;
    let mut host = None;
    let fd = if let Some(cmd) = rest[0].strip_prefix('{').and_then(|c| c.strip_suffix('}')) {
        conn = None;
        let [a, b] = purgatorio::pipe()?;
        purgatorio::spawn(
            "/bin/sh.wasm",
            &[String::from("sh"), String::from("-c"), String::from(cmd)],
            pctl::NEWFD,
            &[(a.as_raw_fd(), 0), (a.as_raw_fd(), 1), (2, 2)],
        )?;
        drop(a);
        pipe = Some(b);
        pipe.as_ref().map_or(-1, |b| b.as_raw_fd())
    } else if rest[0].contains('!') {
        pipe = None;
        host = dial::host(&rest[0]);
        conn = Some(dial::dial(&rest[0])?);
        conn.as_ref()
            .and_then(|c| c.data.as_ref())
            .map(|d| d.as_raw_fd())
            .ok_or_else(|| Error::from("no connection"))?
    } else {
        conn = None;
        pipe = None;
        rest[0].parse().map_err(|_| Error::from("bad fd"))?
    };
    let flags = order | create | noauth | srvauth | kex;
    let kx = purgatorio::auth::Kx::from_alg(alg.as_deref(), flags);
    // The kernel authenticates a plain mount itself, in one conversation,
    // with the default exchange. A keyfile or a guest factotum means the
    // proof is built here, with fauth first, and so does a server proof
    // that must be for a host, and X25519 alone.
    let srvauth = srvauth != 0 || kex & mount::MPQSIG != 0;
    if keyfile.is_some()
        || purgatorio::auth::have_factotum()
        || (srvauth && host.is_some())
        || (kx == purgatorio::auth::Kx::Classical && noauth == 0)
    {
        purgatorio::auth::amount_kx(
            fd,
            &rest[1],
            flags,
            spec,
            keyfile.as_deref(),
            host.as_deref(),
            kx,
        )
    } else {
        purgatorio::mount(fd, -1, &rest[1], flags, spec)
    }?;
    // The mount's session holds the conversation. Dropping `conn` closes this
    // prog's descriptor and leaves the session's reference in place.
    drop(conn);
    drop(pipe);
    Ok(())
}
