//! The `pglogin1` password exchange and password-sealed data.
//!
//! The password is hardened with Argon2id into a 32-byte verifier `w`,
//! which the server keeps (in keyfs). SPAKE2 over the Ed25519 group, with
//! `w` as the password, gives both sides a key only when they used the same
//! `w`. HKDF turns it into one ChaCha20-Poly1305 key per direction.
//!
//! ```text
//! C -> S  "pglogin1 <op> <name>"
//! S -> C  "argon2id <m> <t> <p> <salt>"
//! S -> C  SPAKE2 message B
//! C -> S  SPAKE2 message A
//! C -> S  seal(c2s, request)
//! S -> C  seal(s2c, reply) | "error <reason>"
//! ```
//!
//! Frames are a u32 LE length and that many bytes.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use rand_core::{CryptoRng, RngCore};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::cert::parse_u64;
use crate::hex;

pub const FAILED: &str = purg_abi::auth::FAILED;
pub const EPROTO: &str = "authentication protocol botch";
pub const EBADVERIFIER: &str = "bad verifier";
pub const EKDF: &str = "bad key derivation parameters";

/// The first word of the first frame.
pub const MAGIC: &str = "pglogin1";
/// SPAKE2's identity for the server side.
pub const SERVER_ID: &[u8] = b"pgsigner";
/// The largest frame either side sends or accepts.
pub const MAX_FRAME: usize = 16 * 1024;
/// The SPAKE2 message length for the Ed25519 group.
pub const SPAKE_LEN: usize = 33;
pub const SALT: usize = 16;

/// The default Argon2id cost: the OWASP baseline (19 MiB, 2 passes).
pub const DEFAULT_M: u32 = 19456;
pub const DEFAULT_T: u32 = 2;
/// Limits a client accepts from a server, so a fake signer cannot make it
/// allocate without bound.
pub const MAX_M: u32 = 256 * 1024;
pub const MAX_T: u32 = 16;
pub const MAX_P: u32 = 4;

/// A byte stream: a connection, a pipe.
pub trait Stream {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), String>;
    fn write_all(&mut self, buf: &[u8]) -> Result<(), String>;
}

pub fn write_frame(s: &mut dyn Stream, data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_FRAME {
        return Err(String::from(EPROTO));
    }
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    s.write_all(&buf)
}

pub fn read_frame(s: &mut dyn Stream) -> Result<Vec<u8>, String> {
    let mut n = [0u8; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_le_bytes(n) as usize;
    if n > MAX_FRAME {
        return Err(String::from(EPROTO));
    }
    let mut buf = alloc::vec![0u8; n];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

/// Argon2id parameters and salt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kdf {
    /// Memory in KiB.
    pub m: u32,
    /// Passes.
    pub t: u32,
    /// Lanes.
    pub p: u32,
    pub salt: [u8; SALT],
}

impl Kdf {
    /// Fresh parameters at cost `m`, `t`.
    pub fn new(m: u32, t: u32, rng: &mut (impl RngCore + CryptoRng)) -> Result<Kdf, &'static str> {
        let mut salt = [0u8; SALT];
        rng.fill_bytes(&mut salt);
        let k = Kdf { m, t, p: 1, salt };
        k.params()?;
        Ok(k)
    }

    fn params(&self) -> Result<Params, &'static str> {
        if self.m > MAX_M || self.t > MAX_T || self.p > MAX_P {
            return Err(EKDF);
        }
        Params::new(self.m, self.t, self.p, Some(32)).map_err(|_| EKDF)
    }

    /// Argon2id of `password` under these parameters.
    pub fn derive(&self, password: &[u8]) -> Result<[u8; 32], &'static str> {
        let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, self.params()?);
        let mut out = [0u8; 32];
        a.hash_password_into(password, &self.salt, &mut out)
            .map_err(|_| EKDF)?;
        Ok(out)
    }

    /// `argon2id <m> <t> <p> <salt>`
    pub fn encode(&self) -> String {
        alloc::format!(
            "argon2id {} {} {} {}",
            self.m,
            self.t,
            self.p,
            hex::encode(&self.salt)
        )
    }

    /// Parses [`Kdf::encode`]'s form from the words after `argon2id`.
    fn from_words<'a>(w: &mut impl Iterator<Item = &'a str>) -> Result<Kdf, &'static str> {
        let mut num = || {
            w.next()
                .and_then(parse_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(EKDF)
        };
        let (m, t, p) = (num()?, num()?, num()?);
        let salt = w
            .next()
            .and_then(|s| hex::array::<SALT>(s.as_bytes()))
            .ok_or(EKDF)?;
        let k = Kdf { m, t, p, salt };
        k.params()?;
        Ok(k)
    }

    pub fn parse(text: &[u8]) -> Result<Kdf, &'static str> {
        let text = core::str::from_utf8(text).map_err(|_| EKDF)?;
        let mut w = text.trim_end().split(' ');
        if w.next() != Some("argon2id") {
            return Err(EKDF);
        }
        let k = Kdf::from_words(&mut w)?;
        if w.next().is_some() {
            return Err(EKDF);
        }
        Ok(k)
    }

    /// The binary form used in sealed blobs: m, t, p (u32 LE) and the salt.
    fn to_bytes(self) -> [u8; 12 + SALT] {
        let mut b = [0u8; 12 + SALT];
        b[..4].copy_from_slice(&self.m.to_le_bytes());
        b[4..8].copy_from_slice(&self.t.to_le_bytes());
        b[8..12].copy_from_slice(&self.p.to_le_bytes());
        b[12..].copy_from_slice(&self.salt);
        b
    }

    fn from_bytes(b: &[u8]) -> Result<Kdf, &'static str> {
        let b: &[u8; 12 + SALT] = b.try_into().map_err(|_| EKDF)?;
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let mut salt = [0u8; SALT];
        salt.copy_from_slice(&b[12..]);
        let k = Kdf {
            m: word(0),
            t: word(4),
            p: word(8),
            salt,
        };
        k.params()?;
        Ok(k)
    }
}

/// What the server stores for a user: the parameters and `w`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verifier {
    pub kdf: Kdf,
    pub w: [u8; 32],
}

impl Verifier {
    pub fn new(password: &[u8], kdf: Kdf) -> Result<Verifier, &'static str> {
        Ok(Verifier {
            w: kdf.derive(password)?,
            kdf,
        })
    }

    /// `argon2id <m> <t> <p> <salt> <w>`
    pub fn encode(&self) -> String {
        alloc::format!("{} {}", self.kdf.encode(), hex::encode(&self.w))
    }

    pub fn parse(text: &[u8]) -> Result<Verifier, &'static str> {
        let text = core::str::from_utf8(text).map_err(|_| EBADVERIFIER)?;
        let mut w = text.trim_end().split(' ');
        if w.next() != Some("argon2id") {
            return Err(EBADVERIFIER);
        }
        let kdf = Kdf::from_words(&mut w).map_err(|_| EBADVERIFIER)?;
        let v = w
            .next()
            .and_then(|s| hex::array::<32>(s.as_bytes()))
            .ok_or(EBADVERIFIER)?;
        if w.next().is_some() {
            return Err(EBADVERIFIER);
        }
        Ok(Verifier { kdf, w: v })
    }

    /// A stand-in for a user that does not exist: the salt comes from the
    /// server's `secret` and the name, so it is the same on every attempt,
    /// and `w` is random, so the exchange fails.
    pub fn fake(secret: &[u8], name: &str, rng: &mut (impl RngCore + CryptoRng)) -> Verifier {
        let mut h = Sha256::new();
        h.update(b"pglogin1 fake\0");
        h.update(secret);
        h.update(name.as_bytes());
        let d = h.finalize();
        let mut salt = [0u8; SALT];
        salt.copy_from_slice(&d[..SALT]);
        let mut w = [0u8; 32];
        rng.fill_bytes(&mut w);
        Verifier {
            kdf: Kdf {
                m: DEFAULT_M,
                t: DEFAULT_T,
                p: 1,
                salt,
            },
            w,
        }
    }
}

/// The two directions' keys after a successful exchange.
pub struct Session {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    sent: u64,
    received: u64,
}

fn nonce(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[..8].copy_from_slice(&n.to_le_bytes());
    Nonce::from(b)
}

impl Session {
    fn new(key: &[u8], transcript: &[u8], client: bool) -> Result<Session, String> {
        let hk = Hkdf::<Sha256>::new(Some(transcript), key);
        let mut c2s = [0u8; 32];
        let mut s2c = [0u8; 32];
        hk.expand(b"pglogin1 c2s", &mut c2s)
            .map_err(|_| String::from(EPROTO))?;
        hk.expand(b"pglogin1 s2c", &mut s2c)
            .map_err(|_| String::from(EPROTO))?;
        let c2s = ChaCha20Poly1305::new(&c2s.into());
        let s2c = ChaCha20Poly1305::new(&s2c.into());
        let (send, recv) = if client { (c2s, s2c) } else { (s2c, c2s) };
        Ok(Session {
            send,
            recv,
            sent: 0,
            received: 0,
        })
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let n = nonce(self.sent);
        self.sent += 1;
        self.send.encrypt(&n, plain).unwrap_or_default()
    }

    pub fn open(&mut self, sealed: &[u8]) -> Option<Vec<u8>> {
        let n = nonce(self.received);
        let plain = self.recv.decrypt(&n, sealed).ok()?;
        self.received += 1;
        Some(plain)
    }

    /// Sends one sealed frame.
    pub fn send(&mut self, s: &mut dyn Stream, plain: &[u8]) -> Result<(), String> {
        let sealed = self.seal(plain);
        write_frame(s, &sealed)
    }

    /// Reads one sealed frame. A frame that does not open is an
    /// `error <reason>` in the clear, or a failure.
    pub fn recv(&mut self, s: &mut dyn Stream) -> Result<Vec<u8>, String> {
        let f = read_frame(s)?;
        if let Some(p) = self.open(&f) {
            return Ok(p);
        }
        Err(clear_error(&f).unwrap_or_else(|| String::from(FAILED)))
    }
}

fn clear_error(f: &[u8]) -> Option<String> {
    let s = core::str::from_utf8(f.strip_prefix(b"error ")?).ok()?;
    Some(s.to_string())
}

fn transcript(frames: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for f in frames {
        h.update((f.len() as u32).to_le_bytes());
        h.update(f);
    }
    h.finalize().into()
}

/// The client side. `request` makes the sealed request from the server's
/// parameters (a `passwd` request needs them for the new verifier).
/// Returns the session (for more exchanges) and the server's reply.
pub fn client(
    s: &mut dyn Stream,
    op: &str,
    name: &str,
    password: &[u8],
    rng: &mut (impl RngCore + CryptoRng),
    request: impl FnOnce(&Kdf) -> Result<Vec<u8>, String>,
) -> Result<(Session, Vec<u8>), String> {
    if !crate::cert::valid_name(name) || op.contains(' ') {
        return Err(String::from(crate::cert::EBADNAME));
    }
    let hello = alloc::format!("{MAGIC} {op} {name}");
    write_frame(s, hello.as_bytes())?;
    let params = read_frame(s)?;
    if let Some(e) = clear_error(&params) {
        return Err(e);
    }
    let kdf = Kdf::parse(&params).map_err(String::from)?;
    let b = read_frame(s)?;
    let w = kdf.derive(password).map_err(String::from)?;
    let (st, a) = Spake2::<Ed25519Group>::start_a_with_rng(
        &Password::new(w),
        &Identity::new(name.as_bytes()),
        &Identity::new(SERVER_ID),
        &mut *rng,
    );
    let key = st.finish(&b).map_err(|_| String::from(EPROTO))?;
    write_frame(s, &a)?;
    let mut session = Session::new(&key, &transcript(&[hello.as_bytes(), &params]), true)?;
    let req = request(&kdf)?;
    session.send(s, &req)?;
    let reply = session.recv(s)?;
    Ok((session, reply))
}

/// The first frame, as the server reads it.
pub struct Hello {
    pub op: String,
    pub name: String,
    raw: Vec<u8>,
}

/// Reads the client's first frame.
pub fn server_hello(s: &mut dyn Stream) -> Result<Hello, String> {
    let raw = read_frame(s)?;
    let text = core::str::from_utf8(&raw).map_err(|_| String::from(EPROTO))?;
    let mut w = text.split(' ');
    if w.next() != Some(MAGIC) {
        return Err(String::from(EPROTO));
    }
    let op = w.next().ok_or(EPROTO)?.to_string();
    let name = w.next().ok_or(EPROTO)?.to_string();
    if w.next().is_some() || !crate::cert::valid_name(&name) {
        return Err(String::from(EPROTO));
    }
    Ok(Hello { op, name, raw })
}

/// The server side after [`server_hello`]: runs the exchange against `v`
/// and returns the session and the client's opened request. A wrong
/// password sends `error authentication failed` and fails.
pub fn server(
    s: &mut dyn Stream,
    hello: &Hello,
    v: &Verifier,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(Session, Vec<u8>), String> {
    let params = v.kdf.encode();
    write_frame(s, params.as_bytes())?;
    let (st, b) = Spake2::<Ed25519Group>::start_b_with_rng(
        &Password::new(v.w),
        &Identity::new(hello.name.as_bytes()),
        &Identity::new(SERVER_ID),
        rng,
    );
    write_frame(s, &b)?;
    let a = read_frame(s)?;
    let key = st.finish(&a).map_err(|_| String::from(EPROTO))?;
    let mut session = Session::new(&key, &transcript(&[&hello.raw, params.as_bytes()]), false)?;
    let req = read_frame(s)?;
    match session.open(&req) {
        Some(r) => Ok((session, r)),
        None => {
            let _ = send_error(s, FAILED);
            Err(String::from(FAILED))
        }
    }
}

/// Sends `error <reason>` in the clear.
pub fn send_error(s: &mut dyn Stream, reason: &str) -> Result<(), String> {
    write_frame(s, alloc::format!("error {reason}").as_bytes())
}

/// Password-sealed blobs (keyfs files, secstore files):
/// `MAGIC || kdf (28 bytes) || nonce[12] || ChaCha20-Poly1305(data)`, with
/// everything before the ciphertext as associated data.
pub mod seal {
    use super::*;

    pub const MAGIC: &[u8; 8] = b"pgseal1\0";
    pub const EBAD: &str = "bad sealed file";
    pub const EWRONG: &str = "wrong password or damaged file";
    const HEADER: usize = 8 + 12 + SALT + 12;

    /// Seals `data` under `password` with fresh parameters at cost `m`, `t`.
    pub fn seal(
        password: &[u8],
        m: u32,
        t: u32,
        data: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<Vec<u8>, &'static str> {
        let kdf = Kdf::new(m, t, rng)?;
        let key = kdf.derive(password)?;
        seal_with(&kdf, &key, data, rng)
    }

    /// Seals with an already derived `key` (keyfs keeps its key while it
    /// runs, so a rewrite does not run Argon2 again).
    pub fn seal_with(
        kdf: &Kdf,
        key: &[u8; 32],
        data: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::with_capacity(HEADER + data.len() + 16);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&kdf.to_bytes());
        let mut n = [0u8; 12];
        rng.fill_bytes(&mut n);
        out.extend_from_slice(&n);
        let c = ChaCha20Poly1305::new(key.into());
        let ct = c
            .encrypt(
                &Nonce::from(n),
                Payload {
                    msg: data,
                    aad: &out,
                },
            )
            .map_err(|_| EBAD)?;
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// The parameters a sealed blob was made with.
    pub fn kdf(blob: &[u8]) -> Result<Kdf, &'static str> {
        if blob.len() < HEADER + 16 || &blob[..8] != MAGIC {
            return Err(EBAD);
        }
        Kdf::from_bytes(&blob[8..8 + 12 + SALT])
    }

    /// Opens a blob, returning its data and the key it was sealed under.
    pub fn open(password: &[u8], blob: &[u8]) -> Result<(Vec<u8>, Kdf, [u8; 32]), &'static str> {
        let kdf = kdf(blob)?;
        let key = kdf.derive(password)?;
        let data = open_with(&key, blob)?;
        Ok((data, kdf, key))
    }

    pub fn open_with(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, &'static str> {
        kdf(blob)?;
        let (head, ct) = blob.split_at(HEADER);
        let c = ChaCha20Poly1305::new(key.into());
        c.decrypt(
            &Nonce::try_from(&head[HEADER - 12..]).map_err(|_| EBAD)?,
            Payload { msg: ct, aad: head },
        )
        .map_err(|_| EWRONG)
    }
}

/// Chunked encryption for `crypt`: a header `MAGIC || kdf || prefix[7]`,
/// then chunks of a u32 LE length and ChaCha20-Poly1305 ciphertext. Chunk
/// `i` uses the nonce `prefix || i (u32 BE) || last`, so chunks cannot be
/// reordered, dropped or cut off at the end without detection.
pub mod stream {
    use super::*;

    pub const MAGIC: &[u8; 8] = b"pgcrypt1";
    pub const CHUNK: usize = 64 * 1024;
    pub const HEADER: usize = 8 + 12 + SALT + 7;
    pub const EBAD: &str = "not an encrypted file";
    pub const EWRONG: &str = "wrong secret or damaged file";

    pub struct Cipher {
        aead: ChaCha20Poly1305,
        prefix: [u8; 7],
        next: u32,
    }

    fn nonce(prefix: &[u8; 7], i: u32, last: bool) -> Nonce {
        let mut b = [0u8; 12];
        b[..7].copy_from_slice(prefix);
        b[7..11].copy_from_slice(&i.to_be_bytes());
        b[11] = last as u8;
        Nonce::from(b)
    }

    impl Cipher {
        /// A new encryption: returns the cipher and the header to write.
        pub fn encrypt(
            secret: &[u8],
            m: u32,
            t: u32,
            rng: &mut (impl RngCore + CryptoRng),
        ) -> Result<(Cipher, Vec<u8>), &'static str> {
            let kdf = Kdf::new(m, t, rng)?;
            let key = kdf.derive(secret)?;
            let mut prefix = [0u8; 7];
            rng.fill_bytes(&mut prefix);
            let mut h = Vec::with_capacity(HEADER);
            h.extend_from_slice(MAGIC);
            h.extend_from_slice(&kdf.to_bytes());
            h.extend_from_slice(&prefix);
            Ok((Cipher::with(&key, prefix), h))
        }

        /// Reads a header for decryption.
        pub fn decrypt(secret: &[u8], header: &[u8]) -> Result<Cipher, &'static str> {
            if header.len() != HEADER || &header[..8] != MAGIC {
                return Err(EBAD);
            }
            let kdf = Kdf::from_bytes(&header[8..8 + 12 + SALT])?;
            let key = kdf.derive(secret)?;
            let mut prefix = [0u8; 7];
            prefix.copy_from_slice(&header[HEADER - 7..]);
            Ok(Cipher::with(&key, prefix))
        }

        fn with(key: &[u8; 32], prefix: [u8; 7]) -> Cipher {
            Cipher {
                aead: ChaCha20Poly1305::new(key.into()),
                prefix,
                next: 0,
            }
        }

        /// Seals the next chunk (at most [`CHUNK`] bytes), with its length.
        pub fn seal(&mut self, data: &[u8], last: bool) -> Result<Vec<u8>, &'static str> {
            let n = nonce(&self.prefix, self.next, last);
            self.next = self.next.checked_add(1).ok_or(EBAD)?;
            let ct = self.aead.encrypt(&n, data).map_err(|_| EBAD)?;
            let mut out = Vec::with_capacity(4 + ct.len());
            out.extend_from_slice(&(ct.len() as u32).to_le_bytes());
            out.extend_from_slice(&ct);
            Ok(out)
        }

        /// Opens the next chunk's ciphertext (without its length).
        pub fn open(&mut self, ct: &[u8], last: bool) -> Result<Vec<u8>, &'static str> {
            let n = nonce(&self.prefix, self.next, last);
            self.next = self.next.checked_add(1).ok_or(EBAD)?;
            self.aead.decrypt(&n, ct).map_err(|_| EWRONG)
        }
    }
}

/// `netkey`'s response to `challenge`: the first 8 hex digits of
/// HMAC-SHA256 keyed with SHA-256 of the password.
pub fn netkey_response(password: &[u8], challenge: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let key = Sha256::digest(password);
    let Ok(mut m) = <Hmac<Sha256> as Mac>::new_from_slice(&key) else {
        return String::new();
    };
    m.update(challenge);
    let out = m.finalize().into_bytes();
    hex::encode(&out[..4])
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[test]
    fn netkey() {
        // HMAC-SHA256(SHA256("password"), "12345"), from Python's hmac.
        assert_eq!(super::netkey_response(b"password", b"12345"), "40a002a4");
        assert_ne!(super::netkey_response(b"password", b"12346"), "40a002a4");
    }

    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// A deterministic RNG for tests.
    struct Rng(u64);
    impl RngCore for Rng {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for b in dest {
                *b = (self.next_u64() >> 33) as u8;
            }
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }
    impl CryptoRng for Rng {}

    /// One end of an in-memory duplex pipe.
    struct End {
        tx: Sender<u8>,
        rx: Receiver<u8>,
        buf: VecDeque<u8>,
    }

    impl Stream for End {
        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), String> {
            for b in buf {
                *b = match self.buf.pop_front() {
                    Some(b) => b,
                    None => self.rx.recv().map_err(|_| String::from("hungup"))?,
                };
            }
            Ok(())
        }
        fn write_all(&mut self, buf: &[u8]) -> Result<(), String> {
            for &b in buf {
                self.tx.send(b).map_err(|_| String::from("hungup"))?;
            }
            Ok(())
        }
    }

    fn pipe() -> (End, End) {
        let (a_tx, b_rx) = channel();
        let (b_tx, a_rx) = channel();
        (
            End {
                tx: a_tx,
                rx: a_rx,
                buf: VecDeque::new(),
            },
            End {
                tx: b_tx,
                rx: b_rx,
                buf: VecDeque::new(),
            },
        )
    }

    const M: u32 = 8;
    const T: u32 = 1;

    fn verifier(pw: &[u8]) -> Verifier {
        Verifier::new(pw, Kdf::new(M, T, &mut Rng(1)).unwrap()).unwrap()
    }

    /// Runs a server for one exchange that answers `ok <request>`.
    fn serve(mut s: End, v: Option<Verifier>) -> std::thread::JoinHandle<Result<String, String>> {
        std::thread::spawn(move || {
            let hello = server_hello(&mut s)?;
            let v = v.unwrap_or_else(|| Verifier::fake(b"k", &hello.name, &mut Rng(9)));
            let (mut sess, req) = server(&mut s, &hello, &v, &mut Rng(2))?;
            let mut reply = b"ok ".to_vec();
            reply.extend_from_slice(&req);
            sess.send(&mut s, &reply)?;
            Ok(alloc::format!("{} {}", hello.op, hello.name))
        })
    }

    #[test]
    fn the_right_password_works() {
        let (mut c, s) = pipe();
        let srv = serve(s, Some(verifier(b"secret")));
        let (_, reply) = client(&mut c, "cert", "alice", b"secret", &mut Rng(3), |k| {
            assert_eq!(k.m, M);
            Ok(b"hello".to_vec())
        })
        .unwrap();
        assert_eq!(reply, b"ok hello");
        assert_eq!(srv.join().unwrap().unwrap(), "cert alice");
    }

    #[test]
    fn a_wrong_password_fails_on_both_sides() {
        let (mut c, s) = pipe();
        let srv = serve(s, Some(verifier(b"secret")));
        let r = client(&mut c, "cert", "alice", b"guess", &mut Rng(3), |_| {
            Ok(b"hello".to_vec())
        });
        assert_eq!(r.err().unwrap(), FAILED);
        assert_eq!(srv.join().unwrap().unwrap_err(), FAILED);
    }

    #[test]
    fn an_unknown_user_looks_like_a_wrong_password() {
        let (mut c, s) = pipe();
        let srv = serve(s, None);
        let r = client(&mut c, "cert", "nobody", b"x", &mut Rng(3), |_| {
            Ok(Vec::new())
        });
        assert_eq!(r.err().unwrap(), FAILED);
        assert!(srv.join().unwrap().is_err());
        // The fake salt is the same each time.
        let a = Verifier::fake(b"k", "nobody", &mut Rng(1));
        let b = Verifier::fake(b"k", "nobody", &mut Rng(2));
        assert_eq!(a.kdf, b.kdf);
        assert_ne!(a.w, b.w);
    }

    #[test]
    fn records() {
        let v = verifier(b"pw");
        assert_eq!(Verifier::parse(v.encode().as_bytes()).unwrap(), v);
        assert_eq!(Kdf::parse(v.kdf.encode().as_bytes()).unwrap(), v.kdf);
        assert!(Verifier::parse(b"argon2id 8 1 1 00").is_err());
        assert!(Kdf::parse(b"argon2id 999999999 1 1 00000000000000000000000000000000").is_err());
        assert!(Kdf::parse(b"scrypt 8 1 1 00000000000000000000000000000000").is_err());
        // Argon2id is deterministic for fixed inputs.
        assert_eq!(v.kdf.derive(b"pw").unwrap(), v.w);
        assert_ne!(v.kdf.derive(b"pW").unwrap(), v.w);
    }

    #[test]
    fn sealing() {
        let blob = seal::seal(b"master", M, T, b"data", &mut Rng(5)).unwrap();
        let (data, kdf, key) = seal::open(b"master", &blob).unwrap();
        assert_eq!(data, b"data");
        assert_eq!(seal::open(b"wrong", &blob).unwrap_err(), seal::EWRONG);
        let again = seal::seal_with(&kdf, &key, b"more", &mut Rng(6)).unwrap();
        assert_eq!(seal::open(b"master", &again).unwrap().0, b"more");
        let mut bad = blob.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(seal::open(b"master", &bad).unwrap_err(), seal::EWRONG);
        // The parameters are authenticated too.
        let mut bad = blob.clone();
        bad[8] ^= 1;
        assert!(seal::open(b"master", &bad).is_err());
    }

    #[test]
    fn chunks() {
        let (mut e, h) = stream::Cipher::encrypt(b"k", M, T, &mut Rng(7)).unwrap();
        let a = e.seal(b"one", false).unwrap();
        let b = e.seal(b"two", true).unwrap();
        let mut d = stream::Cipher::decrypt(b"k", &h).unwrap();
        assert_eq!(d.open(&a[4..], false).unwrap(), b"one");
        assert_eq!(d.open(&b[4..], true).unwrap(), b"two");
        // A stream cut before its last chunk does not open as finished.
        let mut d = stream::Cipher::decrypt(b"k", &h).unwrap();
        assert!(d.open(&a[4..], true).is_err());
        // Reordered.
        let mut d = stream::Cipher::decrypt(b"k", &h).unwrap();
        assert!(d.open(&b[4..], false).is_err());
        assert!(stream::Cipher::decrypt(b"x", &h).is_ok());
        let mut d = stream::Cipher::decrypt(b"x", &h).unwrap();
        assert_eq!(d.open(&a[4..], false).unwrap_err(), stream::EWRONG);
    }
}
