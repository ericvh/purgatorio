//! A scaling benchmark for threaded modules (`docs/threads.md`):
//! `cargo xtask thread-scale` runs it with 1 to N threads, in the emu and
//! as a native program built from this same source.
//!
//! threadscale workload threads [size]
//!
//! It prints `workload threads milliseconds checksum`. The time is of the
//! parallel part alone, measured inside the program. The workloads:
//!
//! - `primes`: counts the primes below `size` by trial division. Pure
//!   computation; the threads take chunks of the range from a shared
//!   counter, so a slow core takes fewer.
//! - `memory`: sums a `size`-MiB array 32 times. Bound by memory
//!   bandwidth more than by computation.
//! - `alloc`: each thread makes and drops `size` thousand small vectors.
//!   The allocator's lock is the shared resource.
//! - `lock`: `size` thousand increments of one counter under one mutex,
//!   split between the threads. Nothing can run in parallel; it shows
//!   what contention costs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use purgatorio::{Context, Error};

fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    let mut d = 2;
    while d * d <= n {
        if n.is_multiple_of(d) {
            return false;
        }
        d += 1;
    }
    true
}

/// Runs `work` on `threads` threads and adds up what they return.
fn fan(threads: usize, work: impl Fn(usize) -> u64 + Send + Sync + 'static) -> u64 {
    let work = Arc::new(work);
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let work = work.clone();
            thread::spawn(move || work(i))
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().expect("a worker failed"))
        .fold(0u64, u64::wrapping_add)
}

fn primes(threads: usize, limit: u64) -> u64 {
    const CHUNK: u64 = 2000;
    let next = Arc::new(AtomicU64::new(0));
    fan(threads, move |_| {
        let mut found = 0;
        loop {
            let from = next.fetch_add(CHUNK, Ordering::Relaxed);
            if from >= limit {
                return found;
            }
            for n in from..(from + CHUNK).min(limit) {
                found += u64::from(is_prime(n));
            }
        }
    })
}

fn memory(threads: usize, mib: u64) -> u64 {
    const PASSES: usize = 32;
    let words = (mib << 20) as usize / 8;
    let data: Arc<Vec<u64>> = Arc::new((0..words as u64).collect());
    fan(threads, move |i| {
        let (from, to) = (words * i / threads, words * (i + 1) / threads);
        let mut sum = 0u64;
        for _ in 0..PASSES {
            for w in &data[from..to] {
                sum = sum.wrapping_add(*w);
            }
        }
        sum
    })
}

fn alloc(threads: usize, thousands: u64) -> u64 {
    let each = thousands * 1000 / threads as u64;
    fan(threads, move |i| {
        for k in 0..each {
            let v = vec![k as u8; 64 + (k as usize + i) % 192];
            std::hint::black_box(&v);
        }
        // The same total whatever the split.
        each
    })
}

fn lock(threads: usize, thousands: u64) -> u64 {
    let counter = Arc::new(Mutex::new(0u64));
    let each = thousands * 1000 / threads as u64;
    let shared = counter.clone();
    fan(threads, move |_| {
        for _ in 0..each {
            *shared.lock().expect("the lock") += 1;
        }
        0
    });
    *counter.lock().expect("the lock")
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let usage = || Error::from("usage: threadscale primes|memory|alloc|lock threads [size]");
    let workload = args.get(1).ok_or_else(usage)?.clone();
    let threads: usize = args
        .get(2)
        .and_then(|t| t.parse().ok())
        .filter(|t| *t > 0)
        .ok_or_else(usage)?;
    let size = |default: u64| args.get(3).and_then(|s| s.parse().ok()).unwrap_or(default);
    let start = Instant::now();
    let sum = match workload.as_str() {
        "primes" => primes(threads, size(10_000_000)),
        "memory" => memory(threads, size(256)),
        "alloc" => alloc(threads, size(20_000)),
        "lock" => lock(threads, size(20_000)),
        _ => return Err(usage()),
    };
    println!("{workload} {threads} {} {sum}", start.elapsed().as_millis());
    Ok(())
}
