//! auth/factotum [-m mntpt] [-s authinfo] [authinfo ...]: the
//! authentication agent, with Plan 9's `ctl` and `rpc` files at `mntpt`
//! (default `/mnt/factotum`). Each authinfo named is loaded as an
//! `ed25519` key, and each `crypt -g` key file as an `mlkem768x25519` key;
//! `-s` loads an authinfo as the key for server proofs (`role=server`).
//!
//! `ctl` takes lines of attributes; a name starting with `!` is secret and
//! never read back:
//!
//! ```text
//! key proto=ed25519 [user=<name>] [role=<role>] !seed=<hex> [cert='<pgcert1 line>']
//! key proto=pass server=<addr> user=<name> !password=<password>
//! key proto=mlkem768x25519 !secret=<hex>
//! delkey proto=<proto> [attr=value ...]
//! ```
//!
//! `rpc` is a conversation per open: each write is a request (`start
//! <attrs>`, `write <data>`, `read`) and the next read is its reply (`ok
//! [data]`, `needkey <attrs>`, `error <message>`). For `ed25519`, `start
//! proto=ed25519 role=client user=<u> aname=<a>`, then `write <challenge>`,
//! then `read` gives `ok <proof>`, a mount proof factotum builds and signs
//! itself (`docs/auth.md`); it never signs arbitrary bytes. With
//! `role=modsign`, `write <digest[32] || abi>` then `read` gives `ok
//! <entry>`, a module signature entry (`docs/signing.md`) over a message
//! factotum also builds itself. With `role=server`, `write
//! <transcript[32] || bind[32]>` then `read` gives `ok <server proof>`, a
//! version 3 server's proof for that session (`docs/auth.md`, "Server
//! authentication"), again over a message factotum builds. For `pass`,
//! `start proto=pass server=<s> [user=<u>]`, then `read` gives `ok <user>
//! <password>`, quoted. For `mlkem768x25519` (a `crypt -g` key, loaded
//! from its key file or with `ctl`), `start proto=mlkem768x25519`, then
//! `write <header>`, the header of a file `crypt -r` encrypted to the key,
//! then `read` gives `ok <key>`, that file's 32-byte key
//! (`purg_auth::hybrid`); it decapsulates nothing but `crypt` headers.
//!
//! An `ed25519` key with a `role` (`client`, `server` or `modsign`)
//! serves only that role; one without serves every role. A `start`
//! (whose role is `client` when it names none) takes the first key kept
//! for its role, and else the first key with no role, that matches its
//! other attributes.

use std::collections::BTreeMap;

use authlib::fmt::attr::{self, quote};
use authlib::fmt::{self, cert::Cert, hex, keys::AuthInfo};
use authlib::synth::{self, Files};
use purgatorio::abi::error;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/factotum [-m mntpt] [-s authinfo] [authinfo ...]";

/// The roles an `ed25519` key can be kept for.
const ROLES: &[&str] = &["client", "server", "modsign"];

/// The proto of `crypt`'s hybrid keys.
const KEM: &str = "mlkem768x25519";

enum Key {
    Ed {
        user: Option<String>,
        /// The one role it serves; `None` for every role.
        role: Option<String>,
        seed: [u8; fmt::SEED],
        cert: Option<Cert>,
    },
    Pass {
        server: String,
        user: String,
        password: String,
    },
    /// A hybrid X25519 + ML-KEM-768 key for `crypt`'s public-key mode.
    Kem(Box<fmt::hybrid::Secret>),
}

impl Key {
    fn proto(&self) -> &'static str {
        match self {
            Key::Ed { .. } => "ed25519",
            Key::Pass { .. } => "pass",
            Key::Kem(_) => KEM,
        }
    }

    /// The public attributes, as `ctl` shows them.
    fn public(&self) -> Vec<(String, String)> {
        let mut a = vec![(String::from("proto"), String::from(self.proto()))];
        match self {
            Key::Ed {
                user, role, cert, ..
            } => {
                if let Some(u) = user {
                    a.push((String::from("user"), u.clone()));
                }
                if let Some(r) = role {
                    a.push((String::from("role"), r.clone()));
                }
                if let Some(c) = cert {
                    a.push((String::from("cert"), c.encode()));
                }
            }
            Key::Pass { server, user, .. } => {
                a.push((String::from("server"), server.clone()));
                a.push((String::from("user"), user.clone()));
            }
            Key::Kem(k) => {
                a.push((
                    String::from("id"),
                    hex::encode(&fmt::hybrid::id(&k.public())),
                ));
            }
        }
        a
    }

    fn line(&self) -> String {
        let mut s = String::from("key");
        for (k, v) in self.public() {
            s.push_str(&format!(" {k}={}", quote(&v)));
        }
        s.push_str(match self {
            Key::Ed { .. } => " !seed?",
            Key::Pass { .. } => " !password?",
            Key::Kem(_) => " !secret?",
        });
        s.push('\n');
        s
    }

    /// The role this key is kept for, if one.
    fn role(&self) -> Option<&str> {
        match self {
            Key::Ed { role, .. } => role.as_deref(),
            Key::Pass { .. } | Key::Kem(_) => None,
        }
    }

    /// Whether every attribute in `want` (other than `aname` and secrets)
    /// matches this key. A key with no role matches any `role`.
    fn matches(&self, want: &[(String, String)]) -> bool {
        let have = self.public();
        want.iter().all(|(k, v)| {
            (k == "role" && self.role().is_none_or(|r| r == v))
                || k == "aname"
                || k.starts_with('!')
                || attr::get(&have, k).is_some_and(|h| h == v)
                || (k == "user" && matches!(self, Key::Ed { user: None, .. }))
        })
    }
}

fn parse_key(attrs: &[(String, String)]) -> Result<Key, String> {
    let get = |k: &str| attr::get(attrs, k);
    match get("proto") {
        Some("ed25519") => {
            let seed = get("!seed")
                .and_then(|s| hex::array::<{ fmt::SEED }>(s.as_bytes()))
                .ok_or_else(|| String::from("missing or bad !seed"))?;
            let cert = match get("cert") {
                Some(c) => {
                    let c = Cert::parse(c.as_bytes()).map_err(String::from)?;
                    if c.key != fmt::public_key(&seed) {
                        return Err(String::from("certificate is for another key"));
                    }
                    Some(c)
                }
                None => None,
            };
            let user = get("user")
                .map(String::from)
                .or_else(|| cert.as_ref().map(|c| c.name.clone()));
            let role = match get("role") {
                Some(r) if ROLES.contains(&r) => Some(String::from(r)),
                Some(r) => return Err(format!("unknown role {r}")),
                None => None,
            };
            Ok(Key::Ed {
                user,
                role,
                seed,
                cert,
            })
        }
        Some("pass") => Ok(Key::Pass {
            server: String::from(get("server").ok_or("missing server")?),
            user: String::from(get("user").ok_or("missing user")?),
            password: String::from(get("!password").ok_or("missing !password")?),
        }),
        Some(KEM) => {
            let b = get("!secret")
                .and_then(|s| hex::array::<{ fmt::hybrid::SECRET }>(s.as_bytes()))
                .ok_or_else(|| String::from("missing or bad !secret"))?;
            Ok(Key::Kem(Box::new(fmt::hybrid::Secret::from_bytes(&b))))
        }
        Some(p) => Err(format!("unknown proto {p}")),
        None => Err(String::from("missing proto")),
    }
}

/// One rpc conversation.
#[derive(Default)]
struct Rpc {
    /// The key and the start attributes.
    started: Option<(usize, Vec<(String, String)>)>,
    challenge: Option<Vec<u8>>,
    reply: Vec<u8>,
}

#[derive(Default)]
struct Factotum {
    keys: Vec<Key>,
    rpcs: BTreeMap<u32, Rpc>,
}

impl Factotum {
    fn ctl(&mut self, text: &str) -> Result<(), String> {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
            let attrs = attr::parse(rest).ok_or_else(|| String::from("bad quoting"))?;
            match verb {
                "key" => {
                    let k = parse_key(&attrs)?;
                    self.keys.push(k);
                }
                "delkey" => {
                    let before = self.keys.len();
                    // A role names keys kept for it, not those for any.
                    let role = attr::get(&attrs, "role");
                    self.keys.retain(|k| {
                        !(k.matches(&attrs)
                            && attr::get(&attrs, "proto").is_some()
                            && role.is_none_or(|r| k.role() == Some(r)))
                    });
                    if self.keys.len() == before {
                        return Err(String::from("no key matches"));
                    }
                }
                _ => return Err(format!("unknown verb {verb}")),
            }
        }
        Ok(())
    }

    fn request(&mut self, fid: u32, req: &[u8]) -> Vec<u8> {
        let (verb, arg) = match req.iter().position(|&b| b == b' ') {
            Some(i) => (&req[..i], &req[i + 1..]),
            None => (req, &[][..]),
        };
        let keys = &self.keys;
        let rpc = self.rpcs.entry(fid).or_default();
        match verb {
            b"start" => {
                let Some(attrs) = std::str::from_utf8(arg).ok().and_then(attr::parse) else {
                    return b"error bad start".to_vec();
                };
                let Some(proto) = attr::get(&attrs, "proto") else {
                    return b"error missing proto".to_vec();
                };
                // A key kept for this role first, then one for any role.
                let role = attr::get(&attrs, "role").unwrap_or("client");
                let mut want = attrs.clone();
                want.retain(|(k, _)| k != "role");
                want.push((String::from("role"), String::from(role)));
                let fits = |k: &Key| k.proto() == proto && k.matches(&want);
                let found = keys
                    .iter()
                    .position(|k| fits(k) && k.role() == Some(role))
                    .or_else(|| keys.iter().position(fits));
                *rpc = Rpc::default();
                match found {
                    Some(i) => {
                        rpc.started = Some((i, attrs));
                        b"ok".to_vec()
                    }
                    None => {
                        let mut s = String::from("needkey");
                        for (k, v) in &attrs {
                            if k != "role" && k != "aname" {
                                s.push_str(&format!(" {k}={}", quote(v)));
                            }
                        }
                        s.into_bytes()
                    }
                }
            }
            b"write" => match &rpc.started {
                Some((i, _)) if matches!(keys.get(*i), Some(Key::Ed { .. } | Key::Kem(_))) => {
                    rpc.challenge = Some(arg.to_vec());
                    b"ok".to_vec()
                }
                _ => b"error phase error".to_vec(),
            },
            b"read" => {
                let Some((i, attrs)) = &rpc.started else {
                    return b"error phase error".to_vec();
                };
                match keys.get(*i) {
                    Some(Key::Ed {
                        seed, cert, user, ..
                    }) => {
                        let Some(ch) = &rpc.challenge else {
                            return b"error phase error".to_vec();
                        };
                        match attr::get(attrs, "role") {
                            Some("modsign") => return modsign(seed, cert.as_ref(), ch),
                            Some("server") => return server(seed, cert.as_ref(), ch),
                            _ => {}
                        }
                        let uname = attr::get(attrs, "user")
                            .map(String::from)
                            .or_else(|| user.clone())
                            .unwrap_or_else(authlib::user);
                        let aname = attr::get(attrs, "aname").unwrap_or("");
                        let p = fmt::proof::make(seed, cert.as_ref(), ch, &uname, aname);
                        let mut out = b"ok ".to_vec();
                        out.extend_from_slice(&p);
                        out
                    }
                    Some(Key::Pass { user, password, .. }) => {
                        format!("ok {} {}", quote(user), quote(password)).into_bytes()
                    }
                    Some(Key::Kem(k)) => {
                        let Some(header) = &rpc.challenge else {
                            return b"error phase error".to_vec();
                        };
                        match k.open(header) {
                            Ok(key) => {
                                let mut out = b"ok ".to_vec();
                                out.extend_from_slice(&key);
                                out
                            }
                            Err(e) => format!("error {e}").into_bytes(),
                        }
                    }
                    None => b"error key gone".to_vec(),
                }
            }
            _ => b"error unknown verb".to_vec(),
        }
    }
}

/// Makes a server proof: `request` is the session's transcript and bound
/// value (`purg_auth::server::SESSION` bytes). The message, with its
/// context string, is built here, so this signs nothing but server proofs.
fn server(seed: &[u8; fmt::SEED], cert: Option<&Cert>, request: &[u8]) -> Vec<u8> {
    use fmt::server::{SESSION, assemble, message_for};
    let Some((transcript, bind)) = <&[u8; SESSION]>::try_from(request)
        .ok()
        .and_then(|s| Some((s[..32].try_into().ok()?, s[32..].try_into().ok()?)))
    else {
        return b"error bad session".to_vec();
    };
    let public = fmt::public_key(seed);
    let cert = cert.filter(|c| c.key == public);
    let (kind, body) = fmt::proof::body(cert, &public);
    let sig = fmt::sign(seed, &message_for(transcript, bind, kind, &body));
    let mut out = b"ok ".to_vec();
    out.extend_from_slice(&assemble(kind, &sig, &body));
    out
}

/// Signs a module: `request` is its digest followed by its ABI version.
fn modsign(seed: &[u8; fmt::SEED], cert: Option<&Cert>, request: &[u8]) -> Vec<u8> {
    use fmt::modsig::{DIGEST, Entry};
    let Some((digest, abi)) = request.split_first_chunk::<DIGEST>() else {
        return b"error bad digest".to_vec();
    };
    match Entry::sign(seed, cert.cloned().into_iter().collect(), digest, abi) {
        Ok(e) => {
            let mut out = b"ok ".to_vec();
            out.extend_from_slice(&e.encode());
            out
        }
        Err(e) => format!("error {e}").into_bytes(),
    }
}

impl Files for Factotum {
    fn lookup(&self, path: &[String]) -> Option<bool> {
        match path {
            [] => Some(true),
            [f] if f == "ctl" || f == "rpc" => Some(false),
            _ => None,
        }
    }

    fn list(&self, path: &[String]) -> Vec<String> {
        if path.is_empty() {
            vec![String::from("ctl"), String::from("rpc")]
        } else {
            Vec::new()
        }
    }

    fn open(&mut self, path: &[String], fid: u32, _omode: u8) -> Result<(), String> {
        if path.first().is_some_and(|f| f == "rpc") {
            self.rpcs.insert(fid, Rpc::default());
        }
        Ok(())
    }

    fn read(
        &mut self,
        path: &[String],
        fid: u32,
        offset: u64,
        count: u32,
    ) -> Result<Vec<u8>, String> {
        match path.first().map(String::as_str) {
            Some("ctl") => {
                let text: String = self.keys.iter().map(Key::line).collect();
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(text.len());
                let end = (start + count as usize).min(text.len());
                Ok(text.as_bytes()[start..end].to_vec())
            }
            Some("rpc") => {
                let rpc = self.rpcs.entry(fid).or_default();
                let mut r = std::mem::take(&mut rpc.reply);
                if r.is_empty() {
                    return Err(String::from("no request"));
                }
                r.truncate(count as usize);
                Ok(r)
            }
            _ => Err(String::from(error::ENONEXIST)),
        }
    }

    fn write(
        &mut self,
        path: &[String],
        fid: u32,
        _offset: u64,
        data: &[u8],
    ) -> Result<u32, String> {
        match path.first().map(String::as_str) {
            Some("ctl") => {
                let text = std::str::from_utf8(data).map_err(|_| String::from("bad text"))?;
                self.ctl(text)?;
            }
            Some("rpc") => {
                let reply = self.request(fid, data);
                self.rpcs.entry(fid).or_default().reply = reply;
            }
            _ => return Err(String::from(error::EPERM)),
        }
        Ok(data.len() as u32)
    }

    fn clunk(&mut self, _path: &[String], fid: u32) {
        self.rpcs.remove(&fid);
    }
}

/// The `ctl` line that loads the authinfo at `path`, kept for `role` if
/// given. A `crypt -g` key file loads as an `mlkem768x25519` key.
fn authinfo_line(path: &str, role: Option<&str>) -> Result<String, Error> {
    if role.is_none()
        && let Ok(text) = std::fs::read(path)
        && text.starts_with(fmt::hybrid::SECRET_WORD.as_bytes())
    {
        let k = fmt::hybrid::Secret::parse(&text).map_err(|e| Error(format!("{path}: {e}")))?;
        let line = k.encode();
        let hexpart = line.split_once(' ').map_or("", |(_, h)| h);
        return Ok(format!("key proto={KEM} !secret={hexpart}\n"));
    }
    let info: AuthInfo = purgatorio::auth::read_authinfo(path)?;
    let role = role.map(|r| format!(" role={r}")).unwrap_or_default();
    Ok(format!(
        "key proto=ed25519 user={}{role} !seed={} cert={}\n",
        quote(&info.cert.name),
        hex::encode(&info.seed),
        quote(&info.cert.encode())
    ))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        return synth::run(|keys| {
            let mut f = Factotum::default();
            f.ctl(&String::from_utf8_lossy(&keys)).map_err(Error)?;
            Ok(f)
        });
    }
    let mut arg = Arg::new(args, USAGE);
    let mut mtpt = String::from(purgatorio::auth::FACTOTUM);
    let mut server = String::new();
    while let Some(c) = arg.opt() {
        match c {
            'm' => mtpt = arg.earg()?,
            's' => server.push_str(&authinfo_line(&arg.earg()?, Some("server"))?),
            _ => return Err(arg.usage()),
        }
    }
    let mut keys = String::new();
    for path in arg.argv() {
        keys.push_str(&authinfo_line(&path, None)?);
    }
    keys.push_str(&server);
    if purgatorio::stat(&mtpt).is_err() {
        authlib::mkdirs_for(&format!("{mtpt}/x"))?;
    }
    synth::start("auth/factotum", "--serve", &[], &mtpt, keys.as_bytes())
}
