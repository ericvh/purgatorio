//! zeros [-r] [-v value] [blocksize [count]]: write `count` blocks of
//! `blocksize` bytes (both default 1). Each byte is `value` (hex, default
//! 0); with `-r` the block is random, and every block is the same.

use std::io::Write;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "zeros [-r] [-v value] blocksize [number]";

/// Fills `buf` from WASI `random_get`, which reads the kernel's entropy.
#[cfg(target_family = "wasm")]
fn random(buf: &mut [u8]) {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        fn random_get(buf: *mut u8, len: usize) -> i32;
    }
    // SAFETY: the host writes at most `buf.len()` bytes into `buf`.
    unsafe { random_get(buf.as_mut_ptr(), buf.len()) };
}

/// Host builds (clippy, unit tests) have no WASI import.
#[cfg(not(target_family = "wasm"))]
fn random(buf: &mut [u8]) {
    buf.fill(0x5a);
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut rflag, mut val) = (false, 0u8);
    while let Some(o) = arg.opt() {
        match o {
            'r' => rflag = true,
            'v' => {
                let s = arg.earg()?;
                val = u8::from_str_radix(&s, 16).map_err(|_| arg.usage())?;
            }
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let words = arg.argv();
    let num = |i: usize| -> Result<usize, Error> {
        match words.get(i) {
            None => Ok(1),
            Some(s) => s.parse().map_err(|_| usage.clone()),
        }
    };
    let (bs, n) = (num(0)?, num(1)?);
    if bs == 0 || n == 0 || words.len() > 2 {
        return Err(usage.clone());
    }
    let mut block = vec![val; bs];
    if rflag {
        random(&mut block);
    }
    let mut out = std::io::stdout().lock();
    for _ in 0..n {
        out.write_all(&block)?;
    }
    out.flush()?;
    Ok(())
}
