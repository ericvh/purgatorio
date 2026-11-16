//! lethe [-abc] [-A] [-C alg] [-k keyfile] [-t ms] [-u user] addr mtpt [path]: attach the
//! Lethe tree served at `addr` (`docs/lethe.md`) on `mtpt`, in the style
//! of `mount`. `path` is the subtree to attach (default `/`); the ticket
//! is limited to it. The key is the authinfo `keyfile` if given, else
//! `/usr/<user>/keyring/default`, `/usr/<user>/key`, then the test key.
//! It may be an Ed25519 key or a hybrid Ed25519 + ML-DSA-65 one
//! (`docs/lethe.md`, "Hybrid signatures").
//!
//! `-A` asks for no authentication and no encryption, as `mount -A`
//! does: requests go in the clear to a server started with `-A`, which
//! answers them as the user `none`. It takes no key and no user.
//!
//! The ticket comes from the hybrid X25519 + ML-KEM-768 exchange when
//! the server can do it, and from the classical X25519 one when it
//! cannot (`docs/lethe.md`, "The default exchange"). `-C x25519mlkem768`
//! requires the hybrid exchange, as `mount -C` does: a server that does
//! not offer it fails with `no post-quantum key offered`. `-C x25519`
//! asks only for the classical one.
//!
//! `-t ms` sets how long the device trusts what it learnt about a file
//! (its stat, and that cached data is current) before asking the server
//! again; the default is 1000, and 0 asks every time.
//!
//! The connection is made by `#λ`, which keeps the key to renew its
//! ticket and dials again on its own when a connection fails.

use std::io::{Read, Seek, SeekFrom, Write};

use purgatorio::abi::{mode, mount};
use purgatorio::arg::Arg;
use purgatorio::auth::{self, fmt};
use purgatorio::{Context, Error, dial};

const USAGE: &str =
    "usage: lethe [-abc] [-A] [-C alg] [-k keyfile] [-t ms] [-u user] addr mtpt [path]";

/// The key and certificate to authenticate with.
type Key = (fmt::sig::Secret, Option<fmt::cert::AnyCert>);

fn key(keyfile: Option<&str>, user: &str) -> Result<Key, Error> {
    if let Some(k) = keyfile {
        let info = auth::read_authinfo(&auth::keyfile_path(k, user))?;
        return Ok((info.key, Some(info.cert)));
    }
    let safe = !user.is_empty() && user != "." && user != ".." && !user.contains('/');
    let default = format!("/usr/{user}/keyring/default");
    if safe && let Ok(info) = auth::read_authinfo(&default) {
        return Ok((info.key, Some(info.cert)));
    }
    let mut paths = Vec::new();
    if safe {
        paths.push(format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for p in paths {
        if let Ok(secret) = auth::read_secret(&p) {
            return Ok((secret, None));
        }
    }
    Err(Error::from(purgatorio::abi::auth::NO_KEY))
}

/// `addr` as `tcp!numeric-host!port`, through `/net/cs` when it answers.
fn address(addr: &str) -> Result<String, Error> {
    let addr = dial::netmkaddr(addr);
    if let Ok(t) = dial::translate(&addr)
        && let Some(a) = t.into_iter().find(|a| a.starts_with("tcp!"))
    {
        return Ok(a);
    }
    if addr.starts_with("tcp!") {
        Ok(addr)
    } else {
        Err(Error::from("only tcp is supported"))
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut order, mut create) = (mount::MREPL, 0);
    let (mut keyfile, mut user) = (None, None);
    let mut noauth = false;
    let mut kx = None;
    let mut cache = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'c' => create = mount::MCREATE,
            'A' => noauth = true,
            'C' => {
                kx = Some(match arg.earg()?.as_str() {
                    a @ ("x25519" | "x25519mlkem768") => String::from(a),
                    a => return Err(Error(format!("unknown algorithm: {a}"))),
                })
            }
            'k' => keyfile = Some(arg.earg()?),
            'u' => user = Some(arg.earg()?),
            't' => {
                let ms = arg.earg()?;
                cache = Some(ms.parse::<u64>().map_err(|_| arg.usage())?);
            }
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    // -A is a mode of its own: a key or a user with it is a mistake.
    if rest.len() < 2
        || rest.len() > 3
        || (noauth && (keyfile.is_some() || user.is_some() || kx.is_some()))
    {
        return Err(Error::from(USAGE));
    }
    let mut settings = if noauth {
        format!("dial {}\nnoauth\n", address(&rest[0])?)
    } else {
        let user = user.unwrap_or_else(auth::user);
        let (secret, cert) = key(keyfile.as_deref(), &user)?;
        let mut s = format!(
            "dial {}\nuser {user}\nkey {}\n",
            address(&rest[0])?,
            secret.encode()
        );
        if let Some(c) = cert {
            s.push_str(&format!("cert {}\n", c.encode()));
        }
        s
    };
    if let Some(p) = rest.get(2) {
        settings.push_str(&format!("path {p}\n"));
    }
    if let Some(ms) = cache {
        settings.push_str(&format!("cache {ms}\n"));
    }
    if let Some(kx) = kx {
        settings.push_str(&format!("kx {kx}\n"));
    }
    let mut clone = purgatorio::open("#λ/clone", mode::ORDWR)
        .map(purgatorio::fs::File::from)
        .map_err(|e| Error(format!("#λ/clone: {e}")))?;
    clone.write_all(settings.as_bytes()).map_err(Error::from)?;
    let mut id = String::new();
    clone.seek(SeekFrom::Start(0)).map_err(Error::from)?;
    clone.read_to_string(&mut id).map_err(Error::from)?;
    let src = format!("#λ{}", id.trim());
    purgatorio::bind(&src, &rest[1], order | create)
        .map(drop)
        .map_err(|e| Error(format!("cannot bind {src} on {}: {e}", rest[1])))
}
