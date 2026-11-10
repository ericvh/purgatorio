//! auth/keyfs [-m mntpt] [-C m,t] [keyfile]: serve the signer's key
//! database at `mntpt` (default `/mnt/keys`). The database is `keyfile`
//! (default `/keydb/keys`), sealed with ChaCha20-Poly1305 under an
//! Argon2id key from the master password, read as one line on standard
//! input. A missing keyfile is created empty, with cost `-C m,t`.
//!
//! Each user is a directory holding `secret` (the password verifier that
//! `auth/changelogin` writes), `expire` (seconds since 1970, or `never`)
//! and `status` (`ok` or `disabled`). `mkdir` adds a user and `rm -r`
//! removes one. Every change rewrites the keyfile.

use std::collections::BTreeMap;
use std::io::Write;

use authlib::Rng;
use authlib::fmt::login::{Kdf, Verifier, seal};
use authlib::fmt::{cert::valid_name, hex};
use authlib::synth::{self, Files};
use purgatorio::abi::{error, mode};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/keyfs [-m mntpt] [-C m,t] [keyfile]";
const FILES: [&str; 3] = ["expire", "secret", "status"];
const EFORMAT: &str = "bad format";

#[derive(Clone)]
struct Entry {
    secret: Option<String>,
    expire: u64,
    disabled: bool,
}

struct Db {
    users: BTreeMap<String, Entry>,
    path: String,
    kdf: Kdf,
    key: [u8; 32],
}

impl Db {
    fn parse(text: &str) -> Result<BTreeMap<String, Entry>, Error> {
        let mut users = BTreeMap::new();
        for line in text.lines() {
            let mut w = line.splitn(4, ' ');
            let (Some(name), Some(expire), Some(status), Some(secret)) =
                (w.next(), w.next(), w.next(), w.next())
            else {
                return Err(Error::from(EFORMAT));
            };
            let expire = authlib::expiry(expire)?;
            let secret = (secret != "-").then(|| String::from(secret));
            users.insert(
                String::from(name),
                Entry {
                    secret,
                    expire,
                    disabled: status != "ok",
                },
            );
        }
        Ok(users)
    }

    fn text(&self) -> String {
        let mut s = String::new();
        for (name, e) in &self.users {
            s.push_str(&format!(
                "{name} {} {} {}\n",
                authlib::show_expiry(e.expire),
                if e.disabled { "disabled" } else { "ok" },
                e.secret.as_deref().unwrap_or("-")
            ));
        }
        s
    }

    fn save(&self) -> Result<(), String> {
        let blob = seal::seal_with(&self.kdf, &self.key, self.text().as_bytes(), &mut Rng)
            .map_err(String::from)?;
        let mut f = purgatorio::create(&self.path, mode::OWRITE | mode::OTRUNC, 0o600)
            .map(std::fs::File::from)
            .map_err(|e| e.0)?;
        f.write_all(&blob).map_err(|e| Error::from(e).0)
    }

    fn value(&self, path: &[String]) -> Option<String> {
        let [name, file] = path else {
            return None;
        };
        let e = self.users.get(name)?;
        match file.as_str() {
            "secret" => Some(e.secret.clone().map(|s| s + "\n").unwrap_or_default()),
            "expire" => Some(authlib::show_expiry(e.expire) + "\n"),
            "status" => Some(String::from(if e.disabled { "disabled\n" } else { "ok\n" })),
            _ => None,
        }
    }
}

impl Files for Db {
    fn lookup(&self, path: &[String]) -> Option<bool> {
        match path {
            [] => Some(true),
            [name] => self.users.contains_key(name).then_some(true),
            [name, file] => {
                (self.users.contains_key(name) && FILES.contains(&file.as_str())).then_some(false)
            }
            _ => None,
        }
    }

    fn list(&self, path: &[String]) -> Vec<String> {
        match path {
            [] => self.users.keys().cloned().collect(),
            [_] => FILES.iter().map(|f| String::from(*f)).collect(),
            _ => Vec::new(),
        }
    }

    fn read(
        &mut self,
        path: &[String],
        _fid: u32,
        offset: u64,
        count: u32,
    ) -> Result<Vec<u8>, String> {
        let v = self
            .value(path)
            .ok_or_else(|| String::from(error::ENONEXIST))?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(v.len());
        let end = (start + count as usize).min(v.len());
        Ok(v.as_bytes()[start..end].to_vec())
    }

    fn write(
        &mut self,
        path: &[String],
        _fid: u32,
        _offset: u64,
        data: &[u8],
    ) -> Result<u32, String> {
        let [name, file] = path else {
            return Err(String::from(error::EPERM));
        };
        let text = std::str::from_utf8(data).map_err(|_| String::from(EFORMAT))?;
        let text = text.trim();
        let mut e = self
            .users
            .get(name)
            .cloned()
            .ok_or_else(|| String::from(error::ENONEXIST))?;
        match file.as_str() {
            "secret" => {
                Verifier::parse(text.as_bytes()).map_err(String::from)?;
                e.secret = Some(String::from(text));
            }
            "expire" => e.expire = authlib::expiry(text).map_err(|e| e.0)?,
            "status" => {
                e.disabled = match text {
                    "ok" => false,
                    "disabled" => true,
                    _ => return Err(String::from(EFORMAT)),
                }
            }
            _ => return Err(String::from(error::EPERM)),
        }
        self.users.insert(name.clone(), e);
        self.save()?;
        Ok(data.len() as u32)
    }

    fn create(&mut self, dir: &[String], name: &str, perm: u32) -> Result<(), String> {
        if !dir.is_empty() || perm & purgatorio::DMDIR == 0 {
            return Err(String::from(error::EPERM));
        }
        if !valid_name(name) {
            return Err(String::from(EFORMAT));
        }
        self.users.insert(
            String::from(name),
            Entry {
                secret: None,
                expire: 0,
                disabled: false,
            },
        );
        self.save()
    }

    fn remove(&mut self, path: &[String]) -> Result<(), String> {
        match path {
            [name] => {
                self.users.remove(name);
                self.save()
            }
            // Removing a user's files one by one (as `rm -r` does) is
            // harmless; the directory goes when it is removed.
            [_, _] => Ok(()),
            _ => Err(String::from(error::EPERM)),
        }
    }
}

fn absolute(path: &str) -> Result<String, Error> {
    if path.starts_with('/') || path.starts_with('#') {
        return Ok(String::from(path));
    }
    Ok(format!(
        "{}/{path}",
        purgatorio::dot()?.trim_end_matches('/')
    ))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        return synth::run(|secret| {
            let text = String::from_utf8(secret).map_err(|_| Error::from(EFORMAT))?;
            let mut lines = text.splitn(3, '\n');
            let kdf = Kdf::parse(lines.next().unwrap_or("").as_bytes()).map_err(Error::from)?;
            let key = hex::array::<32>(lines.next().unwrap_or("").as_bytes())
                .ok_or_else(|| Error::from(EFORMAT))?;
            let path = String::from(lines.next().unwrap_or(""));
            let blob =
                std::fs::read(&path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
            let data = seal::open_with(&key, &blob).map_err(Error::from)?;
            let users = Db::parse(&String::from_utf8_lossy(&data))?;
            Ok(Db {
                users,
                path,
                kdf,
                key,
            })
        });
    }
    let mut arg = Arg::new(args, USAGE);
    let mut mtpt = String::from(authlib::KEYS);
    let (mut m, mut t) = (
        authlib::fmt::login::DEFAULT_M,
        authlib::fmt::login::DEFAULT_T,
    );
    while let Some(c) = arg.opt() {
        match c {
            'm' => mtpt = arg.earg()?,
            'C' => (m, t) = authlib::cost(&arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() > 1 {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let path = absolute(rest.first().map(String::as_str).unwrap_or(authlib::KEYDB))?;
    let password = authlib::password("master password")?;
    let (kdf, key) = match std::fs::read(&path) {
        Ok(blob) => {
            let (_, kdf, key) = seal::open(password.as_bytes(), &blob)
                .map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
            (kdf, key)
        }
        Err(_) => {
            let kdf = Kdf::new(m, t, &mut Rng).map_err(Error::from)?;
            let key = kdf.derive(password.as_bytes()).map_err(Error::from)?;
            let db = Db {
                users: BTreeMap::new(),
                path: path.clone(),
                kdf,
                key,
            };
            authlib::mkdirs_for(&path)?;
            db.save()
                .map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
            (kdf, key)
        }
    };
    if purgatorio::stat(&mtpt).is_err() {
        authlib::mkdirs_for(&format!("{mtpt}/x"))?;
    }
    let secret = format!("{}\n{}\n{path}", kdf.encode(), hex::encode(&key));
    synth::start("auth/keyfs", "--serve", &[], &mtpt, secret.as_bytes())
}
