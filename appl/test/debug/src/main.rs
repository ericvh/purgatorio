//! debug trap|spin|nap: a guest for the `stack` and `prof` tests
//! (`docs/debug.md`). Each mode goes through `outer`, `middle` and `inner`,
//! which are never inlined, so that a stack taken inside names them.
//!
//! - `trap` executes an `unreachable` instruction in `inner`;
//! - `spin` computes in `burn`, called from `inner`, for at least 4
//!   rounds and 200 ms: enough for a profiler that samples by time
//!   (wasmtime, every 10 ms) or by instructions (wasmi, every 2^20 units
//!   of fuel) to see `burn` many times;
//! - `nap` sleeps in `inner` until it is killed.

use std::hint::black_box;
use std::time::{Duration, Instant};

use purgatorio::{Context, Error};

#[derive(Clone, Copy)]
enum Mode {
    Trap,
    Spin,
    Nap,
}

/// One round of work: a xorshift generator stepped `n` times.
#[inline(never)]
fn burn(n: u32, mut x: u64) -> u64 {
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    x
}

#[inline(never)]
fn inner(mode: Mode) -> u64 {
    match mode {
        Mode::Trap => {
            #[cfg(target_arch = "wasm32")]
            core::arch::wasm32::unreachable();
            #[cfg(not(target_arch = "wasm32"))]
            std::process::abort();
        }
        Mode::Spin => {
            let start = Instant::now();
            let mut x = black_box(0x9e37_79b9_7f4a_7c15u64);
            let mut rounds = 0;
            while rounds < 4 || start.elapsed() < Duration::from_millis(200) {
                x = burn(black_box(100_000), x);
                rounds += 1;
            }
            x
        }
        Mode::Nap => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
    }
}

#[inline(never)]
fn middle(mode: Mode) -> u64 {
    black_box(inner(black_box(mode))) + 1
}

#[inline(never)]
fn outer(mode: Mode) -> u64 {
    black_box(middle(black_box(mode))) + 1
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mode = match args.get(1).map(String::as_str) {
        Some("trap") => Mode::Trap,
        Some("spin") => Mode::Spin,
        Some("nap") => Mode::Nap,
        _ => return Err(Error::from("usage: debug trap|spin|nap")),
    };
    black_box(outer(mode));
    Ok(())
}
