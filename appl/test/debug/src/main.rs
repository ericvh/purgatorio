//! debug trap|spin|hot|nap|deep n: a guest for the `stack` and `prof` tests
//! (`docs/debug.md`). Each mode goes through `outer`, `middle` and `inner`,
//! which are never inlined, so that a stack taken inside names them.
//!
//! - `trap` executes an `unreachable` instruction in `inner`;
//! - `spin` computes in `burn`, called from `inner`, for [`ROUNDS`]
//!   rounds;
//! - `hot` computes in `burn` until its own `/prog/<pid>/prof` holds
//!   [`SAMPLES`] samples, so that a profile of it does not depend on how
//!   fast the machine is or how often the VM samples (wasmtime by time,
//!   wasmi by instructions). It needs profiling on, and fails with
//!   `fail:no samples` if the samples have not come after [`LIMIT`];
//! - `nap` sleeps in `inner` until it is killed;
//! - `deep n` recurses `n` calls deep in `down`, called from `inner`. A
//!   traced prog takes two frames a call, so this checks that tracing
//!   leaves room for a deep recursion (issue #112).

use std::hint::black_box;
use std::time::{Duration, Instant};

use purgatorio::{Context, Error};

/// The rounds of `spin`.
const ROUNDS: u32 = 4;
/// The samples `hot` waits for: enough that `burn`, where nearly all of
/// its time goes, is the function with the most.
const SAMPLES: u64 = 8;
/// How long `hot` waits for them.
const LIMIT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum Mode {
    Trap,
    Spin,
    Hot,
    Nap,
    Deep(u32),
}

/// Recurses `n` calls deep, each with a frame of its own.
#[inline(never)]
fn down(n: u32) -> u64 {
    if n == 0 {
        return 0;
    }
    let frame = [n as u8; 16];
    black_box(&frame);
    black_box(down(black_box(n - 1))) + u64::from(frame[0])
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
            let mut x = black_box(0x9e37_79b9_7f4a_7c15u64);
            for _ in 0..ROUNDS {
                x = burn(black_box(100_000), x);
            }
            x
        }
        Mode::Hot => {
            let start = Instant::now();
            let mut x = black_box(0x9e37_79b9_7f4a_7c15u64);
            // The count is read every 16 rounds: a tick that falls in the
            // read is taken at the next check point, which is in `inner`,
            // so reading often would put samples there.
            while samples() < SAMPLES {
                if start.elapsed() > LIMIT {
                    eprintln!("debug: fewer than {SAMPLES} samples after {LIMIT:?}");
                    purgatorio::exits("fail:no samples");
                }
                for _ in 0..16 {
                    x = burn(black_box(100_000), x);
                }
            }
            x
        }
        Mode::Deep(n) => down(black_box(n)),
        Mode::Nap => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
    }
}

/// The samples in this prog's own profile so far.
fn samples() -> u64 {
    let path = format!("/prog/{}/prof", purgatorio::pid());
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').next()?.parse::<u64>().ok())
        .sum()
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
        Some("hot") => Mode::Hot,
        Some("nap") => Mode::Nap,
        Some("deep") => match args.get(2).and_then(|n| n.parse().ok()) {
            Some(n) => Mode::Deep(n),
            None => return Err(Error::from("usage: debug deep n")),
        },
        _ => return Err(Error::from("usage: debug trap|spin|hot|nap|deep n")),
    };
    black_box(outer(mode));
    Ok(())
}
