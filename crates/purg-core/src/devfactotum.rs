//! `#¤`: a factotum-like agent. It holds one Ed25519 seed, and optionally a
//! certificate for it, and signs whatever a prog writes to `rpc`. The seed
//! never has to sit in a file the guest can read.
//!
//! `ctl` takes `key <hex>`, a bare 32-byte seed, `cert <pgcert1 line>` or
//! `delkey`, and reads as `key <public hex>` and the certificate line.
//!
//! It holds the host owner's key: only a prog acting as the host owner
//! may open `ctl` or `rpc` (`docs/auth.md`, "Users").

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;
use purg_platform::Platform;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::auth;
use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};
use crate::prog::Procs;

const QROOT: u64 = 0;
const QCTL: u64 = 1;
const QRPC: u64 = 2;

struct Rpc {
    msg: Mutex<Vec<u8>>,
}

pub struct DevFactotum {
    platform: Arc<dyn Platform>,
    procs: Arc<Procs>,
    /// `None` until `ctl` is given a seed.
    key: Mutex<Option<[u8; auth::SEED]>>,
    /// A certificate for the key, loaded with `cert ...`.
    cert: Mutex<Option<String>>,
}

impl DevFactotum {
    pub fn new(platform: Arc<dyn Platform>, procs: Arc<Procs>) -> DevFactotum {
        DevFactotum {
            platform,
            procs,
            key: Mutex::new(None),
            cert: Mutex::new(None),
        }
    }

    fn set_key(&self, data: &[u8]) -> Result<()> {
        // A raw seed is taken whole: its first or last byte may happen to
        // be white space.
        let data = if data.len() == auth::SEED {
            data
        } else {
            trim_ascii(data)
        };
        if data == b"delkey" {
            *self.key.lock() = None;
            *self.cert.lock() = None;
            return Ok(());
        }
        if let Some(line) = data.strip_prefix(b"cert ") {
            let seed = self.key.lock().ok_or(Error::new(auth::NO_KEY))?;
            let cert = auth::Cert::parse(line).map_err(|_| Error::new(error::EBADARG))?;
            if cert.key != auth::public_key(&seed) {
                return Err(Error::new(error::EBADARG));
            }
            *self.cert.lock() = Some(cert.encode());
            return Ok(());
        }
        let seed = if data.len() == auth::SEED {
            let mut seed = [0u8; auth::SEED];
            seed.copy_from_slice(data);
            seed
        } else if let Some(hex) = data.strip_prefix(b"key ") {
            parse_hex(trim_ascii(hex))?
        } else {
            return Err(Error::new(error::EBADARG));
        };
        *self.key.lock() = Some(seed);
        *self.cert.lock() = None;
        Ok(())
    }

    fn proof(&self, msg: &[u8]) -> Result<[u8; auth::PROOF]> {
        let seed = self.key.lock().ok_or(Error::new(auth::NO_KEY))?;
        let signature = auth::sign(&seed, msg);
        let public = auth::public_key(&seed);
        let mut proof = [0u8; auth::PROOF];
        proof[..auth::SIGNATURE].copy_from_slice(&signature);
        proof[auth::SIGNATURE..].copy_from_slice(&public);
        Ok(proof)
    }
}

fn trim_ascii(data: &[u8]) -> &[u8] {
    let mut s = data;
    while s.first().is_some_and(|b| b.is_ascii_whitespace()) {
        s = &s[1..];
    }
    while s.last().is_some_and(|b| b.is_ascii_whitespace()) {
        s = &s[..s.len() - 1];
    }
    s
}

fn parse_hex(hex: &[u8]) -> Result<[u8; auth::SEED]> {
    if hex.len() != auth::SEED * 2 {
        return Err(Error::new(error::EBADARG));
    }
    let mut seed = [0u8; auth::SEED];
    for (i, byte) in seed.iter_mut().enumerate() {
        let hi = hex_val(hex[i * 2])?;
        let lo = hex_val(hex[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(seed)
}

fn hex_val(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::new(error::EBADARG)),
    }
}

impl Dev for DevFactotum {
    fn dc(&self) -> char {
        '¤'
    }

    fn name(&self) -> &'static str {
        "factotum"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QROOT,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QROOT {
            return Err(Error::new(error::ENOTDIR));
        }
        let path = match name {
            "ctl" => QCTL,
            "rpc" => QRPC,
            _ => return Err(Error::new(error::ENONEXIST)),
        };
        Ok(Walked {
            qid: Qid {
                path,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let (name, qtype, perm) = match c.qid.path {
            QROOT => ("#¤", QTDIR, DMDIR | 0o555),
            QCTL => ("ctl", QTFILE, 0o600),
            _ => ("rpc", QTFILE, 0o600),
        };
        let user = self.platform.user();
        Ok(Dir {
            dtype: '¤' as u16,
            qid: Qid {
                path: c.qid.path,
                vers: 0,
                qtype,
            },
            mode: perm,
            name: String::from(name),
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        })
    }

    fn open(&self, c: &mut Chan, _omode: u8) -> Result<()> {
        if c.qid.path != QROOT && !self.procs.is_owner() {
            return Err(Error::new(error::EPERM));
        }
        if c.qid.path == QRPC {
            c.aux = Some(Arc::new(Rpc {
                msg: Mutex::new(Vec::new()),
            }));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        match c.qid.path {
            QCTL => {
                let text = match *self.key.lock() {
                    Some(seed) => {
                        let public = auth::public_key(&seed);
                        let mut s = String::from("key ");
                        for b in public {
                            s.push_str(&alloc::format!("{b:02x}"));
                        }
                        s.push('\n');
                        if let Some(cert) = self.cert.lock().as_ref() {
                            s.push_str(cert);
                            s.push('\n');
                        }
                        s
                    }
                    None => String::from("no key\n"),
                };
                Ok(read_str(text.as_bytes(), buf, offset))
            }
            QRPC => {
                let rpc = c.aux::<Rpc>().ok_or(Error::new(error::EBADUSEFD))?;
                let msg = rpc.msg.lock().clone();
                if msg.is_empty() {
                    return Err(Error::new(error::EBADARG));
                }
                let proof = self.proof(&msg)?;
                Ok(read_str(&proof, buf, offset))
            }
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        if c.qid.path != QROOT {
            return Err(Error::new(error::ENOTDIR));
        }
        let user = self.platform.user();
        let file = |path, name| Dir {
            dtype: '¤' as u16,
            qid: Qid {
                path,
                vers: 0,
                qtype: QTFILE,
            },
            mode: 0o600,
            name: String::from(name),
            uid: user.clone(),
            gid: user.clone(),
            ..Dir::default()
        };
        Ok(alloc::vec![file(QCTL, "ctl"), file(QRPC, "rpc")])
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        match c.qid.path {
            QCTL => {
                self.set_key(buf)?;
                Ok(buf.len())
            }
            QRPC => {
                let rpc = c.aux::<Rpc>().ok_or(Error::new(error::EBADUSEFD))?;
                *rpc.msg.lock() = buf.to_vec();
                Ok(buf.len())
            }
            _ => Err(Error::new(error::EISDIR)),
        }
    }
}
