//! crypt [-d] [-C m,t] (-k secret | -f secretfile): encrypt standard input
//! to standard output, or with `-d` decrypt it. The key is Argon2id of the
//! secret at cost `-C m,t` (default 19456 KiB, 2 passes), and the data is
//! sealed with ChaCha20-Poly1305 in 64 KiB chunks, so a damaged, truncated
//! or reordered file fails to decrypt (`docs/auth.md`).

use std::io::{Read, Write};

use authlib::Rng;
use authlib::fmt::login::{DEFAULT_M, DEFAULT_T, stream};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "crypt [-d] [-C m,t] (-k secret | -f secretfile)";

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

fn encrypt(secret: &[u8], m: u32, t: u32) -> Result<(), Error> {
    let (mut c, header) = stream::Cipher::encrypt(secret, m, t, &mut Rng).map_err(Error::from)?;
    let mut out = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    out.write_all(&header)?;
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

fn decrypt(secret: &[u8]) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    let mut header = [0u8; stream::HEADER];
    if fill(&mut input, &mut header)? != header.len() {
        return Err(Error::from(stream::EBAD));
    }
    let mut c = stream::Cipher::decrypt(secret, &header).map_err(Error::from)?;
    let read_chunk = |input: &mut std::io::StdinLock<'_>| -> Result<Option<Vec<u8>>, Error> {
        let mut len = [0u8; 4];
        match fill(input, &mut len)? {
            0 => return Ok(None),
            4 => {}
            _ => return Err(Error::from(stream::EWRONG)),
        }
        let len = u32::from_le_bytes(len) as usize;
        if len > stream::CHUNK + 16 {
            return Err(Error::from(stream::EWRONG));
        }
        let mut ct = vec![0u8; len];
        if fill(input, &mut ct)? != len {
            return Err(Error::from(stream::EWRONG));
        }
        Ok(Some(ct))
    };
    let mut cur = read_chunk(&mut input)?.ok_or_else(|| Error::from(stream::EWRONG))?;
    loop {
        let next = read_chunk(&mut input)?;
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

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut decrypting = false;
    let mut secret = None;
    let (mut m, mut t) = (DEFAULT_M, DEFAULT_T);
    while let Some(c) = arg.opt() {
        match c {
            'd' => decrypting = true,
            'k' => secret = Some(arg.earg()?.into_bytes()),
            'f' => {
                let path = arg.earg()?;
                let mut s = std::fs::read(&path)
                    .map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
                while s.last() == Some(&b'\n') {
                    s.pop();
                }
                secret = Some(s);
            }
            'C' => (m, t) = authlib::cost(&arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let secret = secret.ok_or_else(|| Error(format!("usage: {USAGE}")))?;
    if decrypting {
        decrypt(&secret)
    } else {
        encrypt(&secret, m, t)
    }
}
