//! auth/aescbc -e|-d [-k key] [-f keyfile]: encrypt standard input to
//! standard output, or with `-d` decrypt it, in the file format of Plan
//! 9's and Inferno's secstore. Without `-k` or `-f` the key is read from
//! `/dev/cons` after a `password:` prompt; a key file's trailing newlines
//! are dropped.
//!
//! The format (version 2):
//!
//! ```text
//! "AES CBC SHA1  2\n" || iv[16] || c0[16] || data || mac[20]
//! ```
//!
//! The AES-128 key is the first 16 bytes of SHA-1 of `"aescbc file"`
//! followed by the password; the MAC is HMAC-SHA1, keyed with MD5 of that
//! key, over `c0 || data`. `c0` is a random block encrypted first, so the
//! chain does not start from a known plaintext. The data is AES-CBC; a
//! final partial block is XORed with the encryption of the last cipher
//! block, so the output is as long as the input. `-d` also reads the
//! older format, which has no header and ends with a block of 16 `X`s.
//!
//! This is for files shared with Plan 9 and Inferno secstores. For files
//! that stay here, `crypt` (ChaCha20-Poly1305 under an Argon2id key) is
//! the stronger choice: this format's key is one SHA-1 of the password.
//!
//! Unlike Inferno's, a damaged or wrongly keyed file writes nothing: the
//! whole input is checked before any of it is written.

use std::io::{Read, Write};

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use sha1::Sha1;

const USAGE: &str = "auth/aescbc -d|-e [-k key] [-f keyfile]";

const BS: usize = 16;
const MACLEN: usize = 20;
const V2HDR: &[u8; BS] = b"AES CBC SHA1  2\n";
const CHECKPAT: &[u8; BS] = b"XXXXXXXXXXXXXXXX";

const EAUTH: &str = "decrypted file failed to authenticate";
const ESHORT: &str = "unexpectedly short read";

/// The AES key and the MAC key a password gives.
fn keys(pass: &[u8]) -> ([u8; BS], [u8; BS]) {
    let mut h = Sha1::new();
    h.update(b"aescbc file");
    h.update(pass);
    let d = h.finalize();
    let mut key = [0u8; BS];
    key.copy_from_slice(&d[..BS]);
    let mut mkey = [0u8; BS];
    mkey.copy_from_slice(&Md5::digest(key));
    (key, mkey)
}

/// CBC over `buf` in place, from `iv`. A final partial block is XORed
/// with the encryption of the chain value, in both directions.
struct Cbc {
    aes: Aes128,
    iv: [u8; BS],
}

impl Cbc {
    fn new(key: &[u8; BS], iv: &[u8; BS]) -> Cbc {
        Cbc {
            aes: Aes128::new(GenericArray::from_slice(key)),
            iv: *iv,
        }
    }

    fn tail(&mut self, rest: &mut [u8]) {
        let mut b = GenericArray::clone_from_slice(&self.iv);
        self.aes.encrypt_block(&mut b);
        self.iv.copy_from_slice(&b);
        for (p, k) in rest.iter_mut().zip(self.iv.iter()) {
            *p ^= k;
        }
    }

    fn encrypt(&mut self, buf: &mut [u8]) {
        let (blocks, rest) = buf.as_chunks_mut::<BS>();
        for blk in blocks {
            for (p, k) in blk.iter_mut().zip(self.iv.iter()) {
                *p ^= k;
            }
            let b = GenericArray::from_mut_slice(blk);
            self.aes.encrypt_block(b);
            self.iv.copy_from_slice(b);
        }
        self.tail(rest);
    }

    fn decrypt(&mut self, buf: &mut [u8]) {
        let (blocks, rest) = buf.as_chunks_mut::<BS>();
        for blk in blocks {
            let c = *blk;
            self.aes.decrypt_block(GenericArray::from_mut_slice(blk));
            for (p, k) in blk.iter_mut().zip(self.iv.iter()) {
                *p ^= k;
            }
            self.iv = c;
        }
        self.tail(rest);
    }
}

fn mac(mkey: &[u8; BS], data: &[u8]) -> [u8; MACLEN] {
    let mut m = <Hmac<Sha1> as Mac>::new_from_slice(mkey).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// Encrypts `plain` under `pass`. `rand` is the IV and the first block.
fn encrypt(pass: &[u8], rand: &[u8; 2 * BS], plain: &[u8]) -> Vec<u8> {
    let (key, mkey) = keys(pass);
    let mut iv = [0u8; BS];
    iv.copy_from_slice(&rand[..BS]);
    let mut out = Vec::with_capacity(3 * BS + plain.len() + MACLEN);
    out.extend_from_slice(V2HDR);
    out.extend_from_slice(&iv);
    out.extend_from_slice(&rand[BS..]);
    out.extend_from_slice(plain);
    let mut c = Cbc::new(&key, &iv);
    c.encrypt(&mut out[2 * BS..]);
    let m = mac(&mkey, &out[2 * BS..]);
    out.extend_from_slice(&m);
    out
}

/// Decrypts a file made by [`encrypt`], or one in the older format.
fn decrypt(pass: &[u8], file: &[u8]) -> Result<Vec<u8>, &'static str> {
    let (key, mkey) = keys(pass);
    if file.len() < BS {
        return Err(ESHORT);
    }
    let mut iv = [0u8; BS];
    if file[..BS] == V2HDR[..] {
        if file.len() < 3 * BS + MACLEN {
            return Err(ESHORT);
        }
        let (body, tag) = file[2 * BS..].split_at(file.len() - 2 * BS - MACLEN);
        let want = mac(&mkey, body);
        // Not a secret comparison worth a constant-time loop: a forger
        // learns nothing it could not learn from the result.
        if want[..] != tag[..] {
            return Err(EAUTH);
        }
        iv.copy_from_slice(&file[BS..2 * BS]);
        let mut buf = body.to_vec();
        Cbc::new(&key, &iv).decrypt(&mut buf);
        Ok(buf.split_off(BS))
    } else {
        // The first block is the IV, and the plaintext ends with CHECKPAT.
        if file.len() < 2 * BS {
            return Err(ESHORT);
        }
        iv.copy_from_slice(&file[..BS]);
        let mut buf = file[BS..].to_vec();
        Cbc::new(&key, &iv).decrypt(&mut buf);
        let n = buf.len() - BS;
        if buf[n..] != CHECKPAT[..] {
            return Err(EAUTH);
        }
        buf.truncate(n);
        Ok(buf)
    }
}

/// Reads a password from `/dev/cons`, with echo off if the console allows.
fn read_password() -> Result<Vec<u8>, Error> {
    let mut cons = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/cons")
        .map_err(|_| Error::from("no key"))?;
    let ctl = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/consctl")
        .and_then(|mut f| f.write_all(b"rawon").map(|_| f));
    if ctl.is_err() {
        eprintln!("aescbc: warning: cannot hide typed password");
    }
    cons.write_all(b"password: ")?;
    let mut pass = Vec::new();
    let mut b = [0u8; 1];
    while cons.read(&mut b)? == 1 {
        match b[0] {
            b'\n' | b'\r' => break,
            8 | 0x7f => {
                pass.pop();
            }
            0x15 => pass.clear(),
            c => pass.push(c),
        }
    }
    cons.write_all(b"\n")?;
    drop(ctl);
    Ok(pass)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut enc = None;
    let mut keyfile = None;
    let mut pass = None;
    while let Some(c) = arg.opt() {
        match c {
            'd' | 'e' if enc.is_none() => enc = Some(c == 'e'),
            'f' => keyfile = Some(arg.earg()?),
            'k' => pass = Some(arg.earg()?.into_bytes()),
            _ => return Err(arg.usage()),
        }
    }
    let Some(enc) = enc else {
        return Err(arg.usage());
    };
    let usage = arg.usage();
    if !arg.argv().is_empty() {
        return Err(usage);
    }
    let pass = match (keyfile, pass) {
        (Some(f), _) => {
            let mut k = std::fs::read(&f)
                .map_err(|e| Error(format!("can't open {f}: {}", Error::from(e))))?;
            while k.last() == Some(&b'\n') {
                k.pop();
            }
            k
        }
        (None, Some(p)) => p,
        (None, None) => read_password()?,
    };
    if pass.is_empty() {
        return Err(Error::from("no key"));
    }
    let mut input = Vec::new();
    std::io::stdin().lock().read_to_end(&mut input)?;
    let out = if enc {
        let mut rand = [0u8; 2 * BS];
        purgatorio::auth::Rng::fill(&mut rand);
        encrypt(&pass, &rand, &input)
    } else {
        decrypt(&pass, &input).map_err(Error::from)?
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&out)?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn roundtrip_every_length() {
        let rand = [7u8; 2 * BS];
        for n in 0..70 {
            let plain: Vec<u8> = (0..n as u8).collect();
            let f = encrypt(b"pw", &rand, &plain);
            assert_eq!(f.len(), 3 * BS + n + MACLEN);
            assert_eq!(decrypt(b"pw", &f).unwrap(), plain);
            assert_eq!(decrypt(b"other", &f), Err(EAUTH));
        }
    }

    #[test]
    fn every_changed_byte_fails() {
        let f = encrypt(b"pw", &[1u8; 2 * BS], b"hello, world, and more\n");
        // The IV only decrypts the random first block, which is dropped,
        // so it is the one part a change does not matter to.
        for i in (0..BS).chain(2 * BS..f.len()) {
            let mut g = f.clone();
            g[i] ^= 0x40;
            assert!(decrypt(b"pw", &g).is_err(), "byte {i}");
        }
        assert_eq!(decrypt(b"pw", &f[..f.len() - 1]), Err(EAUTH));
        assert_eq!(decrypt(b"pw", &f[..3 * BS]), Err(ESHORT));
    }

    // Made with a separate implementation (Python's hashlib and hmac, and
    // `openssl enc -aes-128-ecb` for the block cipher):
    // password "secstore", iv 00..0f, first block 10..1f, and 37 bytes of
    // plaintext, so the final partial block is exercised.
    const PLAIN: &[u8] = b"key proto=pass user=glenda !password\n";
    const V2: &str = "4145532043424320534841312020320a000102030405060708090a0b0c0d0e0ffdb9a0455e0d1b404ed13da4b820f10382049291b419f4755700b5061666be12b476db617c522e3c50d6cceb7af3e07e09e4e9aed163f2bd4bea96332bff2b9a58794102e557713aa2";
    const OLD: &str = "000102030405060708090a0b0c0d0e0fbf6b18287d3cd3aa8069e26ee150a9f97347e5c060b352a29b565765398b59d19971712a5dbd6ad6bd88cce8674c0cb25f94068ba6";

    #[test]
    fn known_answer() {
        let rand: [u8; 2 * BS] = core::array::from_fn(|i| i as u8);
        let want = unhex(V2);
        assert_eq!(encrypt(b"secstore", &rand, PLAIN), want);
        assert_eq!(decrypt(b"secstore", &want).unwrap(), PLAIN);
    }

    #[test]
    fn old_format() {
        let f = unhex(OLD);
        assert_eq!(decrypt(b"secstore", &f).unwrap(), PLAIN);
        assert_eq!(decrypt(b"wrong", &f), Err(EAUTH));
    }

    #[test]
    fn keys_from_the_password() {
        // SHA-1("aescbc filesecstore") and MD5 of its first 16 bytes,
        // computed separately.
        let (k, m) = keys(b"secstore");
        let (wk, wm) = (
            "bbe1c25a351a85b1d376cf6695010039",
            "c306d3ffeda4ae2338945fdd7c1a4367",
        );
        assert_eq!(k.to_vec(), unhex(wk));
        assert_eq!(m.to_vec(), unhex(wm));
    }
}
