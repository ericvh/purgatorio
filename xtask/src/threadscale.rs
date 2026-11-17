//! `cargo xtask thread-scale`: how a threaded guest scales with threads
//! (`docs/threads.md`). It runs `appl/test/threadscale` with 1 to N
//! threads in the release emu (wasmtime), and the same source built as a
//! native program, and prints a table for each workload: the time, the
//! speed-up over one thread, and what the emu costs over native.
//!
//! cargo xtask thread-scale [--max N] [--reps R] [--all]
//!
//! `--max` is the most threads (the host's CPUs by default). Up to 16 it
//! tries every count; above that, or always without `--all`, powers of
//! two and the maximum. `--reps` is the runs per point (3); the median is
//! reported. The results go to `target/thread-scale/results.md` too.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use crate::{Result, build_root, cargo, run, workspace};

const WORKLOADS: &[(&str, &str)] = &[
    (
        "primes",
        "primes below 10,000,000 by trial division: computation alone",
    ),
    ("memory", "32 sums of a 256 MiB array: memory bandwidth"),
    (
        "alloc",
        "20,000,000 small allocations: the allocator's lock",
    ),
    (
        "lock",
        "20,000,000 increments under one mutex: pure contention",
    ),
];

/// One run: the milliseconds the program reports.
fn once(cmd: &mut Command) -> Result<f64> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_ascii_whitespace()
        .nth(2)
        .and_then(|ms| ms.parse().ok())
        .ok_or_else(|| {
            format!(
                "{cmd:?}: {} {text:?} {:?}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
        })
}

fn median(cmd: &mut dyn FnMut() -> Command, reps: usize) -> Result<f64> {
    let mut times = Vec::new();
    for _ in 0..reps {
        times.push(once(&mut cmd())?);
    }
    times.sort_by(f64::total_cmp);
    Ok(times[times.len() / 2])
}

fn counts(max: usize, all: bool) -> Vec<usize> {
    if all && max <= 16 {
        return (1..=max).collect();
    }
    let mut out: Vec<usize> = std::iter::successors(Some(1), |n| Some(n * 2))
        .take_while(|n| *n <= max)
        .collect();
    if out.last() != Some(&max) {
        out.push(max);
    }
    out
}

pub fn thread_scale(args: &[String]) -> Result<()> {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let (mut max, mut reps, mut all) = (cpus, 3usize, cpus <= 16);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut number = |what: &str| {
            it.next()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("thread-scale: {what} needs a number"))
        };
        match a.as_str() {
            "--max" => max = number("--max")?,
            "--reps" => reps = number("--reps")?,
            "--all" => all = true,
            _ => return Err("usage: cargo xtask thread-scale [--max N] [--reps R] [--all]".into()),
        }
    }
    let ws = workspace();
    build_root()?;
    run(cargo().current_dir(&ws).args([
        "build",
        "--release",
        "-p",
        "purg-emu",
        "-p",
        "appl-test-threadscale",
    ]))?;
    let emu = ws.join("target/release/emu");
    let native = ws.join("target/release/threadscale");
    let root = ws.join("root");
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# thread-scale\n\n{} CPUs, {} {}; {reps} runs a point, medians; times in ms.\n",
        cpus,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    for (name, what) in WORKLOADS {
        let _ = writeln!(text, "## {name}\n\n{what}.\n");
        let _ = writeln!(
            text,
            "| threads | emu ms | emu speed-up | native ms | native speed-up | emu / native |"
        );
        let _ = writeln!(text, "|---:|---:|---:|---:|---:|---:|");
        let mut first: Option<(f64, f64)> = None;
        for n in counts(max, all) {
            let threads = n.to_string();
            let e = median(
                &mut || {
                    let mut c = Command::new(&emu);
                    c.arg("-r").arg(&root).args([
                        "-x",
                        "/bin/test/threadscale.wasm",
                        name,
                        &threads,
                    ]);
                    c
                },
                reps,
            )?;
            let v = median(
                &mut || {
                    let mut c = Command::new(&native);
                    c.args([name, threads.as_str()]);
                    c
                },
                reps,
            )?;
            let (e1, v1) = *first.get_or_insert((e, v));
            let _ = writeln!(
                text,
                "| {n} | {e:.0} | {:.2} | {v:.0} | {:.2} | {:.2} |",
                e1 / e.max(1.0),
                v1 / v.max(1.0),
                e / v.max(1.0)
            );
            eprintln!("thread-scale: {name} {n}: emu {e:.0} ms, native {v:.0} ms");
        }
        text.push('\n');
    }
    print!("{text}");
    save(&ws, &text)
}

fn save(ws: &Path, text: &str) -> Result<()> {
    let dir = ws.join("target/thread-scale");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("results.md");
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}
