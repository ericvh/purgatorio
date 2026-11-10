//! auth/factotum [-m mntpt] [authinfo ...]: the authentication agent, with
//! Plan 9's `ctl` and `rpc` files at `mntpt` (default `/mnt/factotum`).
//! Each authinfo named is loaded as an `ed25519` key.
//!
//! `ctl` takes lines of attributes; a name starting with `!` is secret and
//! never read back:
//!
//! ```text
//! key proto=ed25519 [user=<name>] !seed=<hex> [cert='<pgcert1 line>']
//! key proto=pass server=<addr> user=<name> !password=<password>
//! delkey proto=<proto> [attr=value ...]
//! ```
//!
//! `rpc` is a conversation per open: each write is a request (`start
//! <attrs>`, `write <data>`, `read`) and the next read is its reply (`ok
//! [data]`, `needkey <attrs>`, `error <message>`). For `ed25519`, `start
//! proto=ed25519 role=client user=<u> aname=<a>`, then `write <challenge>`,
//! then `read` gives `ok <proof>`, a mount proof factotum builds and signs
//! itself (`docs/auth.md`); it never signs arbitrary bytes. For `pass`,
//! `start proto=pass server=<s> [user=<u>]`, then `read` gives `ok <user>
//! <password>`, quoted.

use std::collections::BTreeMap;

use authlib::fmt::attr::{self, quote};
use authlib::fmt::{self, cert::Cert, hex, keys::AuthInfo};
use authlib::synth::{self, Files};
use purgatorio::abi::error;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/factotum [-m mntpt] [authinfo ...]";

enum Key {
    Ed {
        user: Option<String>,
        seed: [u8; fmt::SEED],
        cert: Option<Cert>,
    },
    Pass {
        server: String,
        user: String,
        password: String,
    },
}

impl Key {
    fn proto(&self) -> &'static str {
        match self {
            Key::Ed { .. } => "ed25519",
            Key::Pass { .. } => "pass",
        }
    }

    /// The public attributes, as `ctl` shows them.
    fn public(&self) -> Vec<(String, String)> {
        let mut a = vec![(String::from("proto"), String::from(self.proto()))];
        match self {
            Key::Ed { user, cert, .. } => {
                if let Some(u) = user {
                    a.push((String::from("user"), u.clone()));
                }
                if let Some(c) = cert {
                    a.push((String::from("cert"), c.encode()));
                }
            }
            Key::Pass { server, user, .. } => {
                a.push((String::from("server"), server.clone()));
                a.push((String::from("user"), user.clone()));
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
        });
        s.push('\n');
        s
    }

    /// Whether every attribute in `want` (other than `proto`'s own
    /// wildcard, `role` and secrets) matches this key.
    fn matches(&self, want: &[(String, String)]) -> bool {
        let have = self.public();
        want.iter().all(|(k, v)| {
            k == "role"
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
            Ok(Key::Ed { user, seed, cert })
        }
        Some("pass") => Ok(Key::Pass {
            server: String::from(get("server").ok_or("missing server")?),
            user: String::from(get("user").ok_or("missing user")?),
            password: String::from(get("!password").ok_or("missing !password")?),
        }),
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
                    self.keys
                        .retain(|k| !(k.matches(&attrs) && attr::get(&attrs, "proto").is_some()));
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
                let found = keys
                    .iter()
                    .position(|k| k.proto() == proto && k.matches(&attrs));
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
                Some((i, _)) if matches!(keys.get(*i), Some(Key::Ed { .. })) => {
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
                    Some(Key::Ed { seed, cert, user }) => {
                        let Some(ch) = &rpc.challenge else {
                            return b"error phase error".to_vec();
                        };
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
                    None => b"error key gone".to_vec(),
                }
            }
            _ => b"error unknown verb".to_vec(),
        }
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

fn authinfo_line(path: &str) -> Result<String, Error> {
    let info: AuthInfo = purgatorio::auth::read_authinfo(path)?;
    Ok(format!(
        "key proto=ed25519 user={} !seed={} cert={}\n",
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
    while let Some(c) = arg.opt() {
        match c {
            'm' => mtpt = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let mut keys = String::new();
    for path in arg.argv() {
        keys.push_str(&authinfo_line(&path)?);
    }
    if purgatorio::stat(&mtpt).is_err() {
        authlib::mkdirs_for(&format!("{mtpt}/x"))?;
    }
    synth::start("auth/factotum", "--serve", &[], &mtpt, keys.as_bytes())
}
