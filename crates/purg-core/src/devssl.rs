//! `#D`: a TLS 1.3 client. `clone` allocates a conversation, `ctl` names the
//! connection underneath it and the server's P-256 key, and `client` runs
//! the handshake. `data` is then the decrypted byte stream.
//!
//! The pinned key is the check on `CertificateVerify`. A peer that does not
//! hold that key cannot finish the handshake.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use embedded_io::{ErrorType, Read, Write};
use embedded_tls::blocking::{
    Aes128GcmSha256, CertificateRef, CertificateVerifyRef, CryptoProvider, TlsConfig,
    TlsConnection, TlsContext, TlsError, TlsVerifier,
};
use embedded_tls::{CryptoRng, CryptoRngCore, SignatureScheme};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use purg_abi::{error, mode};
use purg_platform::Platform;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};
use rand_core::RngCore;
use sha2::{Digest, Sha256};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};
use crate::prog::Procs;

const QROOT: u64 = 0;
const QCLONE: u64 = 1;
const BASE: u64 = 1 << 40;
const FDIR: u64 = 0;
const FCTL: u64 = 1;
const FDATA: u64 = 2;
const READ_BUF: usize = 16640;
const WRITE_BUF: usize = 4096;

fn qconv(id: u32, file: u64) -> u64 {
    BASE | (u64::from(id) << 8) | file
}

fn conv_of(path: u64) -> Option<(u32, u64)> {
    if path & BASE == 0 {
        return None;
    }
    Some((((path >> 8) & 0xffff_ffff) as u32, path & 0xff))
}

struct ChanIo {
    chan: Arc<Chan>,
}

#[derive(Debug)]
struct IoErr;

impl core::fmt::Display for IoErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("i/o error")
    }
}

impl core::error::Error for IoErr {}

impl embedded_io::Error for IoErr {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl ErrorType for ChanIo {
    type Error = IoErr;
}

impl Read for ChanIo {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, IoErr> {
        self.chan.dev.read(&self.chan, buf, 0).map_err(|_| IoErr)
    }
}

impl Write for ChanIo {
    fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, IoErr> {
        self.chan.dev.write(&self.chan, buf, 0).map_err(|_| IoErr)
    }

    fn flush(&mut self) -> core::result::Result<(), IoErr> {
        Ok(())
    }
}

struct EntropyRng {
    platform: Arc<dyn Platform>,
}

impl RngCore for EntropyRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }

    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.platform.entropy(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for EntropyRng {}

struct PinVerifier {
    key: VerifyingKey,
    transcript: Option<[u8; 32]>,
}

impl TlsVerifier<Aes128GcmSha256> for PinVerifier {
    fn set_hostname_verification(&mut self, _hostname: &str) -> core::result::Result<(), TlsError> {
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        transcript: &Sha256,
        _cert: CertificateRef,
    ) -> core::result::Result<(), TlsError> {
        let digest = transcript.clone().finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        self.transcript = Some(hash);
        Ok(())
    }

    fn verify_signature(
        &mut self,
        verify: CertificateVerifyRef,
    ) -> core::result::Result<(), TlsError> {
        if verify.signature_scheme != SignatureScheme::EcdsaSecp256r1Sha256 {
            return Err(TlsError::InvalidSignatureScheme);
        }
        let Some(hash) = self.transcript else {
            return Err(TlsError::InvalidSignature);
        };
        let mut content = Vec::with_capacity(64 + 33 + 32);
        content.extend(core::iter::repeat_n(b' ', 64));
        content.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
        content.extend_from_slice(&hash);
        let signature =
            Signature::from_der(verify.signature).map_err(|_| TlsError::InvalidSignature)?;
        self.key
            .verify(&content, &signature)
            .map_err(|_| TlsError::InvalidSignature)
    }
}

struct Provider {
    rng: EntropyRng,
    pin: PinVerifier,
}

impl CryptoProvider for Provider {
    type CipherSuite = Aes128GcmSha256;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(
        &mut self,
    ) -> core::result::Result<&mut impl TlsVerifier<Aes128GcmSha256>, TlsError> {
        Ok(&mut self.pin)
    }
}

/// Owns the record buffers and the connection that borrows them.
struct Live {
    read_buf: *mut [u8],
    write_buf: *mut [u8],
    tls: *mut TlsConnection<'static, ChanIo, Aes128GcmSha256>,
}

// The pointers are an exclusive heap allocation. The mutex that holds `Live`
// is what makes sharing it sound.
unsafe impl Send for Live {}

impl Drop for Live {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.tls));
            drop(Box::from_raw(self.read_buf));
            drop(Box::from_raw(self.write_buf));
        }
    }
}

impl Live {
    fn open(
        io: ChanIo,
        server: &str,
        pubkey: &[u8; 65],
        platform: &Arc<dyn Platform>,
    ) -> Result<Live> {
        let key = VerifyingKey::from_sec1_bytes(pubkey).map_err(|_| Error::new(error::EBADARG))?;
        let read_buf = alloc::vec![0u8; READ_BUF].into_boxed_slice();
        let write_buf = alloc::vec![0u8; WRITE_BUF].into_boxed_slice();
        let read_buf = Box::into_raw(read_buf);
        let write_buf = Box::into_raw(write_buf);
        let opened = (|| -> core::result::Result<_, Error> {
            let read_slice: &'static mut [u8] = unsafe { &mut *read_buf };
            let write_slice: &'static mut [u8] = unsafe { &mut *write_buf };
            let mut tls = Box::new(TlsConnection::new(io, read_slice, write_slice));
            let config = TlsConfig::new().with_server_name(server);
            let provider = Provider {
                rng: EntropyRng {
                    platform: platform.clone(),
                },
                pin: PinVerifier {
                    key,
                    transcript: None,
                },
            };
            tls.open(TlsContext::new(&config, provider))
                .map_err(|e| Error::from(alloc::format!("tls: {e:?}")))?;
            Ok(Box::into_raw(tls))
        })();
        match opened {
            Ok(tls) => Ok(Live {
                read_buf,
                write_buf,
                tls,
            }),
            Err(e) => {
                unsafe {
                    drop(Box::from_raw(read_buf));
                    drop(Box::from_raw(write_buf));
                }
                Err(e)
            }
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        unsafe { &mut *self.tls }
            .read(buf)
            .map_err(|e| Error::from(alloc::format!("tls: {e:?}")))
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let tls = unsafe { &mut *self.tls };
        let n = tls
            .write(buf)
            .map_err(|e| Error::from(alloc::format!("tls: {e:?}")))?;
        tls.flush()
            .map_err(|e| Error::from(alloc::format!("tls: {e:?}")))?;
        Ok(n)
    }
}

struct Conv {
    id: u32,
    under: Mutex<Option<Arc<Chan>>>,
    server: Mutex<String>,
    pubkey: Mutex<Option<[u8; 65]>>,
    tls: Mutex<Option<Live>>,
}

pub struct DevSsl {
    platform: Arc<dyn Platform>,
    procs: Arc<Procs>,
    next: AtomicU32,
    convs: Mutex<BTreeMap<u32, Arc<Conv>>>,
}

impl DevSsl {
    pub fn new(platform: Arc<dyn Platform>, procs: Arc<Procs>) -> DevSsl {
        DevSsl {
            platform,
            procs,
            next: AtomicU32::new(0),
            convs: Mutex::new(BTreeMap::new()),
        }
    }

    fn alloc(&self) -> Arc<Conv> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let conv = Arc::new(Conv {
            id,
            under: Mutex::new(None),
            server: Mutex::new(String::from("purgatorio")),
            pubkey: Mutex::new(None),
            tls: Mutex::new(None),
        });
        self.convs.lock().insert(id, conv.clone());
        conv
    }

    fn conv(&self, id: u32) -> Result<Arc<Conv>> {
        self.convs
            .lock()
            .get(&id)
            .cloned()
            .ok_or(Error::new(error::ENONEXIST))
    }

    fn command(&self, conv: &Conv, text: &str) -> Result<()> {
        let mut parts = text.split_whitespace();
        match parts.next() {
            Some("fd") => {
                let fd: i32 = parts
                    .next()
                    .ok_or(Error::new(error::EBADARG))?
                    .parse()
                    .map_err(|_| Error::new(error::EBADARG))?;
                let chan = self.procs.current()?.fgrp().get(fd)?;
                *conv.under.lock() = Some(chan);
                Ok(())
            }
            Some("server") => {
                let name = parts.next().ok_or(Error::new(error::EBADARG))?;
                *conv.server.lock() = String::from(name);
                Ok(())
            }
            Some("pubkey") => {
                let hex = parts.next().ok_or(Error::new(error::EBADARG))?;
                *conv.pubkey.lock() = Some(parse_point(hex)?);
                Ok(())
            }
            Some("client") => self.handshake(conv),
            _ => Err(Error::new(error::EBADARG)),
        }
    }

    fn handshake(&self, conv: &Conv) -> Result<()> {
        let chan = conv.under.lock().clone().ok_or(Error::new("no fd"))?;
        let server = conv.server.lock().clone();
        let pubkey = conv.pubkey.lock().ok_or(Error::new(error::EBADARG))?;
        let live = Live::open(ChanIo { chan }, &server, &pubkey, &self.platform)?;
        *conv.tls.lock() = Some(live);
        Ok(())
    }
}

fn parse_point(hex: &str) -> Result<[u8; 65]> {
    if hex.len() != 130 {
        return Err(Error::new(error::EBADARG));
    }
    let mut out = [0u8; 65];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_val(hex.as_bytes()[i * 2])?;
        let lo = hex_val(hex.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::new(error::EBADARG)),
    }
}

impl Dev for DevSsl {
    fn dc(&self) -> char {
        'D'
    }

    fn name(&self) -> &'static str {
        "ssl"
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
        let (path, qtype) = if c.qid.path == QROOT {
            if name == "clone" {
                (QCLONE, QTFILE)
            } else {
                let id: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                self.conv(id)?;
                (qconv(id, FDIR), QTDIR)
            }
        } else if let Some((id, FDIR)) = conv_of(c.qid.path) {
            let file = match name {
                "ctl" => FCTL,
                "data" => FDATA,
                _ => return Err(Error::new(error::ENONEXIST)),
            };
            (qconv(id, file), QTFILE)
        } else {
            return Err(Error::new(error::ENOTDIR));
        };
        Ok(Walked {
            qid: Qid {
                path,
                vers: 0,
                qtype,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let (name, _qtype, perm) = match c.qid.path {
            QROOT => (String::from("#D"), QTDIR, DMDIR | 0o555),
            QCLONE => (String::from("clone"), QTFILE, 0o666),
            path => match conv_of(path) {
                Some((id, FDIR)) => (id.to_string(), QTDIR, DMDIR | 0o555),
                Some((_, FCTL)) => (String::from("ctl"), QTFILE, 0o666),
                Some((_, FDATA)) => (String::from("data"), QTFILE, 0o666),
                _ => return Err(Error::new(error::ENONEXIST)),
            },
        };
        let user = self.platform.user();
        Ok(Dir {
            dtype: 'D' as u16,
            qid: c.qid,
            mode: perm,
            name,
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        })
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QROOT || conv_of(c.qid.path).is_some_and(|(_, f)| f == FDIR) {
            if omode & mode::ACCESS != mode::OREAD {
                return Err(Error::new(error::EISDIR));
            }
            return Ok(());
        }
        if c.qid.path == QCLONE {
            let conv = self.alloc();
            c.qid = Qid {
                path: qconv(conv.id, FCTL),
                vers: 0,
                qtype: QTFILE,
            };
            c.name = alloc::format!("#D/{}/ctl", conv.id);
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        match conv_of(c.qid.path) {
            Some((id, FCTL)) => {
                let conv = self.conv(id)?;
                let state = if conv.tls.lock().is_some() {
                    "ready"
                } else {
                    "idle"
                };
                let text = alloc::format!("{id} {state}\n");
                Ok(read_str(text.as_bytes(), buf, offset))
            }
            Some((id, FDATA)) => {
                let conv = self.conv(id)?;
                let mut tls = conv.tls.lock();
                let live = tls.as_mut().ok_or(Error::new("tls not open"))?;
                live.read(buf)
            }
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let user = self.platform.user();
        let file = |path, name: &str, dir: bool| Dir {
            dtype: 'D' as u16,
            qid: Qid {
                path,
                vers: 0,
                qtype: if dir { QTDIR } else { QTFILE },
            },
            mode: if dir { DMDIR | 0o555 } else { 0o666 },
            name: String::from(name),
            uid: user.clone(),
            gid: user.clone(),
            ..Dir::default()
        };
        if c.qid.path == QROOT {
            let mut out = alloc::vec![file(QCLONE, "clone", false)];
            for id in self.convs.lock().keys() {
                out.push(file(qconv(*id, FDIR), &id.to_string(), true));
            }
            return Ok(out);
        }
        let Some((id, FDIR)) = conv_of(c.qid.path) else {
            return Err(Error::new(error::ENOTDIR));
        };
        self.conv(id)?;
        Ok(alloc::vec![
            file(qconv(id, FCTL), "ctl", false),
            file(qconv(id, FDATA), "data", false),
        ])
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        let Some((id, file)) = conv_of(c.qid.path) else {
            return Err(Error::new(error::EBADARG));
        };
        let conv = self.conv(id)?;
        match file {
            FCTL => {
                let text = core::str::from_utf8(buf).map_err(|_| Error::new(error::EBADARG))?;
                self.command(&conv, text.trim())?;
                Ok(buf.len())
            }
            FDATA => {
                let mut tls = conv.tls.lock();
                let live = tls.as_mut().ok_or(Error::new("tls not open"))?;
                live.write(buf)
            }
            _ => Err(Error::new(error::EISDIR)),
        }
    }
}
