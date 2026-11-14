//! crypt [-d] [-C m,t] (-k secret | -f secretfile): encrypt standard input
//! to standard output, or with `-d` decrypt it. The key is Argon2id of the
//! secret at cost `-C m,t` (default 19456 KiB, 2 passes), and the data is
//! sealed with ChaCha20-Poly1305 in 64 KiB chunks, so a damaged, truncated
//! or reordered file fails to decrypt (`docs/auth.md`).
//!
//! Public-key mode (an addition; `docs/auth.md`, "crypt's public-key
//! mode"): `crypt -g keyfile` makes a hybrid X25519 + ML-KEM-768 key pair,
//! writes the secret key to `keyfile` and prints the public key line.
//! `crypt -r pubkey` encrypts to the public key in the file `pubkey` (or
//! its secret key file). `crypt -d -s keyfile` decrypts with the secret
//! key in `keyfile`; `crypt -d` with no key decrypts such a file with the
//! key loaded into the guest factotum, which never hands out the secret.

use std::io::{Read, Write};

use authlib::Rng;
use authlib::fmt::hybrid;
use authlib::fmt::login::{DEFAULT_M, DEFAULT_T, stream};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str =
    "crypt [-d] [-C m,t] [-k secret | -f secretfile | -r pubkey | -s keyfile | -g keyfile]";

/// Reads up to `buf.len()` bytes, stopping only at end of file.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> Result<usize, Error> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Seals standard input to standard output in chunks, after `header`.
fn seal_all(mut c: stream::Cipher, header: &[u8]) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    out.write_all(header)?;
    let mut cur = vec![0u8; stream::CHUNK];
    let mut n = fill(&mut input, &mut cur)?;
    loop {
        let mut next = vec![0u8; stream::CHUNK];
        let k = if n == stream::CHUNK {
            fill(&mut input, &mut next)?
        } else {
            0
        };
        let last = k == 0;
        out.write_all(&c.seal(&cur[..n], last).map_err(Error::from)?)?;
        if last {
            break;
        }
        cur = next;
        n = k;
    }
    out.flush()?;
    Ok(())
}

fn encrypt(secret: &[u8], m: u32, t: u32) -> Result<(), Error> {
    let (c, header) = stream::Cipher::encrypt(secret, m, t, &mut Rng).map_err(Error::from)?;
    seal_all(c, &header)
}

fn encrypt_to(recipient: &[u8; hybrid::PUBLIC]) -> Result<(), Error> {
    let (header, key, prefix) = hybrid::seal(recipient, &mut Rng).map_err(Error::from)?;
    seal_all(stream::Cipher::with_key(&key, prefix), &header)
}

/// Opens the chunks on standard input, after the header, to standard
/// output. A wrong key fails on the first chunk.
fn open_all(mut c: stream::Cipher, input: &mut impl Read) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();
    let read_chunk = |input: &mut dyn Read| -> Result<Option<Vec<u8>>, Error> {
        let mut len = [0u8; 4];
        let mut r = input;
        match fill(&mut r, &mut len)? {
            0 => return Ok(None),
            4 => {}
            _ => return Err(Error::from(stream::EWRONG)),
        }
        let len = u32::from_le_bytes(len) as usize;
        if len > stream::CHUNK + 16 {
            return Err(Error::from(stream::EWRONG));
        }
        let mut ct = vec![0u8; len];
        if fill(&mut r, &mut ct)? != len {
            return Err(Error::from(stream::EWRONG));
        }
        Ok(Some(ct))
    };
    let mut cur = read_chunk(input)?.ok_or_else(|| Error::from(stream::EWRONG))?;
    loop {
        let next = read_chunk(input)?;
        let plain = c.open(&cur, next.is_none()).map_err(Error::from)?;
        out.write_all(&plain)?;
        match next {
            Some(n) => cur = n,
            None => break,
        }
    }
    out.flush()?;
    Ok(())
}

/// The key a file starts with: a password, or a hybrid secret key from a
/// file or from factotum.
enum Key {
    Secret(Vec<u8>),
    File(Box<hybrid::Secret>),
    Factotum,
}

fn decrypt(key: Option<Key>) -> Result<(), Error> {
    let mut input = std::io::stdin().lock();
    let mut magic = [0u8; 8];
    if fill(&mut input, &mut magic)? != magic.len() {
        return Err(Error::from(stream::EBAD));
    }
    if &magic == hybrid::MAGIC {
        let mut header = vec![0u8; hybrid::HEADER];
        header[..8].copy_from_slice(&magic);
        if fill(&mut input, &mut header[8..])? != hybrid::HEADER - 8 {
            return Err(Error::from(hybrid::EBAD));
        }
        let prefix = hybrid::Header::parse(&header).map_err(Error::from)?.prefix;
        let file_key = match key {
            Some(Key::File(k)) => k.open(&header).map_err(Error::from)?,
            None | Some(Key::Factotum) => factotum_key(&header)?,
            Some(Key::Secret(_)) => return Err(Error::from(ENEEDKEY)),
        };
        return open_all(stream::Cipher::with_key(&file_key, prefix), &mut input);
    }
    let Some(Key::Secret(secret)) = key else {
        return Err(Error::from(if &magic == stream::MAGIC {
            ENEEDSECRET
        } else {
            stream::EBAD
        }));
    };
    let mut header = [0u8; stream::HEADER];
    header[..8].copy_from_slice(&magic);
    if fill(&mut input, &mut header[8..])? != header.len() - 8 {
        return Err(Error::from(stream::EBAD));
    }
    let c = stream::Cipher::decrypt(&secret, &header).map_err(Error::from)?;
    open_all(c, &mut input)
}

/// A file encrypted to a key, decrypted with a secret.
const ENEEDKEY: &str = "file is encrypted to a key, not a secret";
/// A file encrypted with a secret, decrypted without one.
const ENEEDSECRET: &str = "file is encrypted with a secret";

/// Asks the guest factotum for the key of the file whose header is
/// `header` (`auth/factotum`'s `mlkem768x25519` keys).
fn factotum_key(header: &[u8]) -> Result<[u8; 32], Error> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{}/rpc", purgatorio::auth::FACTOTUM))
        .map_err(|_| Error::from(purgatorio::abi::auth::NO_KEY))?;
    let mut rpc = |req: &[u8]| -> Result<Vec<u8>, Error> {
        f.write_all(req)?;
        let mut buf = vec![0u8; 4096];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    };
    if rpc(b"start proto=mlkem768x25519")? != b"ok" {
        return Err(Error::from(purgatorio::abi::auth::NO_KEY));
    }
    let mut w = b"write ".to_vec();
    w.extend_from_slice(header);
    if rpc(&w)? != b"ok" {
        return Err(Error::from(purgatorio::abi::auth::NO_KEY));
    }
    let r = rpc(b"read")?;
    if let Some(k) = r.strip_prefix(b"ok ") {
        return k
            .try_into()
            .map_err(|_| Error::from(purgatorio::abi::auth::NO_KEY));
    }
    let e = r.strip_prefix(b"error ").unwrap_or(&r);
    Err(Error(String::from_utf8_lossy(e).into_owned()))
}

/// Reads a key file.
fn read_key(path: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))
}

/// A recipient's public key: its public key line, or its secret key file.
fn recipient(path: &str) -> Result<[u8; hybrid::PUBLIC], Error> {
    let text = read_key(path)?;
    if text.starts_with(hybrid::SECRET_WORD.as_bytes()) {
        let k = hybrid::Secret::parse(&text).map_err(|e| Error(format!("{path}: {e}")))?;
        return Ok(k.public());
    }
    hybrid::parse_public(&text).map_err(|e| Error(format!("{path}: {e}")))
}

fn generate(path: &str) -> Result<(), Error> {
    let k = hybrid::Secret::generate(&mut Rng);
    authlib::mkdirs_for(path)?;
    let mut line = k.encode();
    line.push('\n');
    purgatorio::auth::write_secret(path, line.as_bytes())?;
    println!("{}", hybrid::encode_public(&k.public()));
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // Inferno's crypt ends a bad command line with `fail: bad usage`.
    run(args).map_err(|e| e.usage_exits("usage: ", "fail: bad usage"))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut decrypting = false;
    let mut key = None;
    let mut to = None;
    let mut make = None;
    let (mut m, mut t) = (DEFAULT_M, DEFAULT_T);
    let usage = || Error(format!("usage: {USAGE}"));
    while let Some(c) = arg.opt() {
        let set = |slot: &mut Option<Key>, k: Key| {
            if slot.is_some() {
                return Err(usage());
            }
            *slot = Some(k);
            Ok(())
        };
        match c {
            'd' => decrypting = true,
            'k' => set(&mut key, Key::Secret(arg.earg()?.into_bytes()))?,
            'f' => {
                let path = arg.earg()?;
                let mut s = read_key(&path)?;
                while s.last() == Some(&b'\n') {
                    s.pop();
                }
                set(&mut key, Key::Secret(s))?;
            }
            's' => {
                let path = arg.earg()?;
                let k = hybrid::Secret::parse(&read_key(&path)?)
                    .map_err(|e| Error(format!("{path}: {e}")))?;
                set(&mut key, Key::File(Box::new(k)))?;
            }
            'r' => to = Some(arg.earg()?),
            'g' => make = Some(arg.earg()?),
            'C' => (m, t) = authlib::cost(&arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(usage());
    }
    match (decrypting, key, to, make) {
        (false, None, None, Some(path)) => generate(&path),
        (false, None, Some(path), None) => encrypt_to(&recipient(&path)?),
        (false, Some(Key::Secret(s)), None, None) => encrypt(&s, m, t),
        (true, key, None, None) => decrypt(key.or(Some(Key::Factotum))),
        _ => Err(usage()),
    }
}
