//! randpass [length]: print a random password of `length` characters
//! (more than 8, at most 256; default 16) drawn from the base64 alphabet.

use purgatorio::{Context, Error};
use purgatorio::{eprintln, println};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Fills `buf` from the kernel's entropy.
#[cfg(target_family = "wasm")]
fn random(buf: &mut [u8]) {
    purgatorio::auth::Rng::fill(buf);
}

/// Host builds (clippy, unit tests) have no WASI import.
#[cfg(not(target_family = "wasm"))]
fn random(buf: &mut [u8]) {
    buf.fill(0x5a);
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let len = match args.get(1) {
        None => 16,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if s.bytes().all(|b| b.is_ascii_digit()) && n > 8 && n <= 256 => n,
            _ => {
                eprintln!("Usage: randpass [password-length(<256, default=16)]");
                purgatorio::exits("fail:usage");
            }
        },
    };
    let mut bytes = vec![0u8; len];
    random(&mut bytes);
    let pw: String = bytes
        .iter()
        .map(|b| B64[usize::from(b & 63)] as char)
        .collect();
    println!("{pw}");
    Ok(())
}
