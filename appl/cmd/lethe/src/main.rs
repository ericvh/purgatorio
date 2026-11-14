//! lethe [-abc] [-A] [-C alg] [-k keyfile] [-t ms] [-u user] addr mtpt [path]: attach the
//! Lethe tree served at `addr` (`docs/lethe.md`) on `mtpt`, in the style
//! of `mount`. `path` is the subtree to attach (default `/`); the ticket
//! is limited to it. The key is the authinfo `keyfile` if given, else
//! `/usr/<user>/keyring/default`, `/usr/<user>/key`, then the test key.
//!
//! `-A` asks for no authentication and no encryption, as `mount -A`
//! does: requests go in the clear to a server started with `-A`, which
//! answers them as the user `none`. It takes no key and no user.
//!
//! `-C x25519mlkem768` uses and requires the hybrid X25519 + ML-KEM-768
//! exchange for the ticket (`docs/lethe.md`, "The post-quantum
//! exchange"), as `mount -C` does; a server that does not offer it fails
//! with `no post-quantum key offered`. `-C x25519` names the default.
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

/// The seed and certificate to authenticate with.
fn key(keyfile: Option<&str>, user: &str) -> Result<([u8; 32], Option<fmt::cert::Cert>), Error> {
    // Lethe's exchange is Ed25519 only (`docs/lethe.md`, Section 6).
    let ed = |path: &str, info: fmt::keys::AuthInfo| {
        info.ed25519()
            .map(|(s, c)| (s, Some(c)))
            .ok_or_else(|| Error(format!("{path}: Lethe takes only an Ed25519 key")))
    };
    if let Some(k) = keyfile {
        let path = auth::keyfile_path(k, user);
        return ed(&path, auth::read_authinfo(&path)?);
    }
    let safe = !user.is_empty() && user != "." && user != ".." && !user.contains('/');
    let default = format!("/usr/{user}/keyring/default");
    if safe
        && let Ok(info) = auth::read_authinfo(&default)
        && info.ed25519().is_some()
    {
        return ed(&default, info);
    }
    let mut paths = Vec::new();
    if safe {
        paths.push(format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for p in paths {
        if let Ok(seed) = auth::read_seed(&p) {
            return Ok((seed, None));
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
    let mut pq = false;
    let mut cache = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => order = mount::MAFTER,
            'b' => order = mount::MBEFORE,
            'c' => create = mount::MCREATE,
            'A' => noauth = true,
            'C' => {
                pq = match arg.earg()?.as_str() {
                    "x25519" => false,
                    "x25519mlkem768" => true,
                    a => return Err(Error(format!("unknown algorithm: {a}"))),
                }
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
    if rest.len() < 2 || rest.len() > 3 || (noauth && (keyfile.is_some() || user.is_some() || pq)) {
        return Err(Error::from(USAGE));
    }
    let mut settings = if noauth {
        format!("dial {}\nnoauth\n", address(&rest[0])?)
    } else {
        let user = user.unwrap_or_else(auth::user);
        let (seed, cert) = key(keyfile.as_deref(), &user)?;
        let mut s = format!(
            "dial {}\nuser {user}\nkey {}\n",
            address(&rest[0])?,
            fmt::hex::encode(&seed)
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
    if pq {
        settings.push_str("kx x25519mlkem768\n");
    }
    let mut clone = purgatorio::open("#λ/clone", mode::ORDWR)
        .map(std::fs::File::from)
        .map_err(|e| Error(format!("#λ/clone: {e}")))?;
    clone.write_all(settings.as_bytes()).map_err(Error::from)?;
    let mut id = String::new();
    clone.seek(SeekFrom::Start(0)).map_err(Error::from)?;
    clone.read_to_string(&mut id).map_err(Error::from)?;
    let src = format!("#λ{}", id.trim());
    purgatorio::bind(&src, &rest[1], order | create)
        .map_err(|e| Error(format!("cannot bind {src} on {}: {e}", rest[1])))
}
