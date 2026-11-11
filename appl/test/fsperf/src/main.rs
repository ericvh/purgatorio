//! fsperf [-c ctl] [-n files] [-k reps] mtpt case...: times file system
//! workloads on the tree mounted on `mtpt`, for the Lethe and Styx study
//! (`docs/lethe-perf.md`). `cargo xtask lethe-perf` makes the tree and runs
//! this; it is not part of any test.
//!
//! Each case runs twice, `cold` and then `warm`, and prints one line per
//! run:
//!
//! ```text
//! R case phase ms ops c2s-frames c2s-bytes s2c-frames s2c-bytes [extra]
//! ```
//!
//! With `-c tcp!host!port`, the counts come from the counting relay's
//! control port, which answers each connection with one line of totals
//! (`c2s-frames c2s-bytes s2c-frames s2c-bytes`) and hangs up; without it
//! they are `-`. The latency cases add the median and the 90th percentile
//! of one operation in microseconds.
//!
//! The cases: `read1m` and `read100m` (a whole file in 64 KiB reads),
//! `small` (open, read and close `files` 1 KiB files), `stat` (`files`
//! names), `walk` (stat a name 17 elements deep, `reps` times), `readdir`
//! (list a directory of `files` entries), `lsl` (list it and stat the
//! first `files` entries), `du` (list a tree of directories recursively), `lat` (open,
//! read and close a small file, `reps` times), `latread` (read an open
//! file at offset 0, `reps` times) and `restart` (one slow read of
//! `big100m` while the harness restarts the server; see the doc).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "fsperf [-c ctl] [-n files] [-k reps] mtpt case...";

/// The depth of `deep/d01/.../d16/leaf`.
pub const DEPTH: usize = 16;

struct Bench {
    mtpt: String,
    ctl: Option<String>,
    files: usize,
    reps: usize,
}

/// What a run did besides its time.
struct Done {
    ops: usize,
    extra: String,
}

impl Done {
    fn ops(ops: usize) -> Done {
        Done {
            ops,
            extra: String::new(),
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut b = Bench {
        mtpt: String::new(),
        ctl: None,
        files: 10_000,
        reps: 100,
    };
    let num = |s: String| s.parse::<usize>().map_err(|_| Error::from("bad number"));
    while let Some(c) = arg.opt() {
        match c {
            'c' => b.ctl = Some(arg.earg()?),
            'n' => b.files = num(arg.earg()?)?,
            'k' => b.reps = num(arg.earg()?)?,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let Some((mtpt, cases)) = rest.split_first() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    b.mtpt = mtpt.clone();
    for case in cases {
        if case == "restart" {
            b.restart();
            continue;
        }
        for phase in ["cold", "warm"] {
            let before = b.counts();
            let start = Instant::now();
            let r = b.run(case);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let after = b.counts();
            match r {
                Ok(d) => {
                    let counts = match (before, after) {
                        (Some(a), Some(z)) => {
                            let d: Vec<String> = z
                                .iter()
                                .zip(a.iter())
                                .map(|(z, a)| (z - a).to_string())
                                .collect();
                            d.join(" ")
                        }
                        _ => String::from("- - - -"),
                    };
                    say(&format!(
                        "R {case} {phase} {ms:.3} {} {counts}{}",
                        d.ops, d.extra
                    ));
                }
                Err(e) => say(&format!("E {case} {phase} {e}")),
            }
        }
    }
    Ok(())
}

fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Reads a whole file in 64 KiB reads; returns its length.
fn slurp(path: &str, buf: &mut [u8]) -> Result<u64, Error> {
    let mut f = File::from(purgatorio::open(path, mode::OREAD)?);
    let mut n = 0u64;
    loop {
        let k = f.read(buf)?;
        if k == 0 {
            return Ok(n);
        }
        n += k as u64;
    }
}

/// Every entry of the directory `path`.
fn list(path: &str) -> Result<Vec<purgatorio::Dir>, Error> {
    let fd = purgatorio::open(path, mode::OREAD)?;
    let mut all = Vec::new();
    loop {
        let d = purgatorio::dirread(std::os::fd::AsRawFd::as_raw_fd(&fd))?;
        if d.is_empty() {
            return Ok(all);
        }
        all.extend(d);
    }
}

/// The median and 90th percentile of `t`, in microseconds.
fn spread(mut t: Vec<Duration>) -> String {
    t.sort();
    let at = |q: f64| {
        let i = ((t.len() as f64 - 1.0) * q).round() as usize;
        t.get(i).map_or(0.0, |d| d.as_secs_f64() * 1e6)
    };
    format!(" {:.1} {:.1}", at(0.5), at(0.9))
}

impl Bench {
    fn p(&self, rel: &str) -> String {
        format!("{}/{rel}", self.mtpt)
    }

    fn small(&self, i: usize) -> String {
        self.p(&format!("small/f{i:05}"))
    }

    fn deep(&self) -> String {
        let mut s = self.p("deep");
        for i in 1..=DEPTH {
            s.push_str(&format!("/d{i:02}"));
        }
        s.push_str("/leaf");
        s
    }

    /// The relay's totals, if there is one.
    fn counts(&self) -> Option<[u64; 4]> {
        let ctl = self.ctl.as_ref()?;
        let c = purgatorio::dial::dial_translated(ctl).ok()?;
        let mut data = File::from(c.data?);
        let mut s = String::new();
        data.read_to_string(&mut s).ok()?;
        let v: Vec<u64> = s
            .split_whitespace()
            .filter_map(|w| w.parse().ok())
            .collect();
        (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
    }

    fn run(&self, case: &str) -> Result<Done, Error> {
        let mut buf = vec![0u8; 64 * 1024];
        match case {
            "read1m" | "read100m" => {
                let name = if case == "read1m" { "big1m" } else { "big100m" };
                let n = slurp(&self.p(name), &mut buf)?;
                Ok(Done {
                    ops: 1,
                    extra: format!(" {n}"),
                })
            }
            "small" => {
                for i in 0..self.files {
                    let n = slurp(&self.small(i), &mut buf)?;
                    if n != 1024 {
                        return Err(Error(format!("small {i}: {n} bytes")));
                    }
                }
                Ok(Done::ops(self.files))
            }
            "stat" => {
                for i in 0..self.files {
                    purgatorio::stat(&self.small(i))?;
                }
                Ok(Done::ops(self.files))
            }
            "walk" => {
                let deep = self.deep();
                for _ in 0..self.reps {
                    purgatorio::stat(&deep)?;
                }
                Ok(Done::ops(self.reps))
            }
            "readdir" => {
                let n = list(&self.p("small"))?.len();
                if n < self.files {
                    return Err(Error(format!("readdir: {n} entries")));
                }
                Ok(Done::ops(n))
            }
            "lsl" => {
                let dir = self.p("small");
                purgatorio::stat(&dir)?;
                let all = list(&dir)?;
                let n = all.len().min(self.files);
                for d in &all[..n] {
                    purgatorio::stat(&format!("{dir}/{}", d.name))?;
                }
                Ok(Done::ops(n))
            }
            "du" => {
                let mut stack = vec![self.p("tree")];
                let (mut entries, mut bytes) = (0usize, 0u64);
                while let Some(dir) = stack.pop() {
                    for d in list(&dir)? {
                        entries += 1;
                        if d.qid.is_dir() {
                            stack.push(format!("{dir}/{}", d.name));
                        } else {
                            bytes += d.length;
                        }
                    }
                }
                Ok(Done {
                    ops: entries,
                    extra: format!(" {bytes}"),
                })
            }
            "lat" => {
                let one = self.p("one");
                let mut t = Vec::with_capacity(self.reps);
                for _ in 0..self.reps {
                    let s = Instant::now();
                    slurp(&one, &mut buf)?;
                    t.push(s.elapsed());
                }
                Ok(Done {
                    ops: self.reps,
                    extra: spread(t),
                })
            }
            "latread" => {
                let mut f = File::from(purgatorio::open(&self.p("one"), mode::OREAD)?);
                let mut t = Vec::with_capacity(self.reps);
                for _ in 0..self.reps {
                    let s = Instant::now();
                    f.seek(SeekFrom::Start(0))?;
                    let n = f.read(&mut buf[..64])?;
                    t.push(s.elapsed());
                    if n == 0 {
                        return Err(Error::from("latread: empty"));
                    }
                }
                Ok(Done {
                    ops: self.reps,
                    extra: spread(t),
                })
            }
            _ => Err(Error(format!("unknown case {case}"))),
        }
    }

    /// Reads `big100m` while the harness restarts the server, saying
    /// `reading` first; reports the read's outcome, its longest single
    /// read, and then whether the mount still answers.
    fn restart(&self) {
        say("reading");
        let start = Instant::now();
        let mut longest = Duration::ZERO;
        let mut buf = vec![0u8; 64 * 1024];
        let mut n = 0u64;
        let r: Result<(), Error> = (|| {
            let mut f = File::from(purgatorio::open(&self.p("big100m"), mode::OREAD)?);
            loop {
                let s = Instant::now();
                let k = f.read(&mut buf)?;
                longest = longest.max(s.elapsed());
                if k == 0 {
                    return Ok(());
                }
                n += k as u64;
            }
        })();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let longest = longest.as_secs_f64() * 1000.0;
        match r {
            Ok(()) => say(&format!(
                "restart read ok {n} bytes in {ms:.0} ms, longest read {longest:.0} ms"
            )),
            Err(e) => say(&format!(
                "restart read failed after {n} bytes, {ms:.0} ms: {e}"
            )),
        }
        match purgatorio::stat(&self.p("one")) {
            Ok(_) => say("restart then stat ok"),
            Err(e) => say(&format!("restart then stat failed: {e}")),
        }
    }
}
