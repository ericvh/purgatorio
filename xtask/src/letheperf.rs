//! `cargo xtask lethe-perf`: the Lethe and Styx study (`docs/lethe-perf.md`).
//!
//! It makes a tree under `root/tmp/lethe-perf/srv`, serves it read-only
//! three ways (an `export -e` from a second emu, authenticated and
//! encrypted; an `export -A` in the clear; `lethed`), and runs
//! `/bin/test/fsperf.wasm` against each from a client emu, or from the
//! kernel under KVM. A host relay between client and server counts frames
//! and bytes each way and can delay them, to emulate a longer round trip.
//! Results go to `target/lethe-perf/`; a Markdown summary is printed.
//!
//! ```text
//! cargo xtask lethe-perf [--reps N] [--parts direct,wasmi,counts,sweep,restart,kernel]
//! ```
//!
//! It is a study, not a test: nothing checks the numbers, and nothing in
//! CI runs it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    GUEST_MAC, PROBE_TARGET, Result, cargo, elf_to_image, free_port, initrd, run, workspace,
};

/// The tree's place in the guest's namespace (`#U` serves `root/`).
const GUEST_TREE: &str = "/tmp/lethe-perf/srv";
/// The client's mount point.
const MTPT: &str = "/tmp/lethe-perf/n";
/// Bump when the tree's shape changes, so it is made again.
const TREE_STAMP: &str = "lethe-perf tree 1\n";
/// Files in `small/`, which is also the large directory.
const SMALL: usize = 10_000;
/// The kernel's root on the host, as QEMU user networking shows it.
const KERNEL_HOST: &str = "10.0.2.2";

/// The cases of a full run, in order.
const CASES: &[&str] = &[
    "lat", "latread", "walk", "stat", "readdir", "lsl", "du", "small", "read1m", "read100m",
];
/// The cases of the latency sweep, with fewer files and repetitions.
const SWEEP_CASES: &[&str] = &[
    "lat", "latread", "walk", "stat", "readdir", "lsl", "small", "read1m",
];
/// Round trip times for the sweep, in milliseconds.
const SWEEP_RTT_MS: &[u64] = &[1, 10, 50];

/// How a client reaches a server.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Proto {
    /// `mount` against `export -e`: authenticated, ChaCha20-Poly1305 records.
    StyxSealed,
    /// `mount -A` against `export -A`: no proof, in the clear.
    StyxClear,
    /// `lethe` against `lethed`.
    Lethe,
    /// `lethe -A` against `lethed -A`: no ticket, in the clear. Run only
    /// when this tree's `lethed` takes `-A` (branch `lethe-clear`).
    LetheClear,
}

const ALL: [Proto; 4] = [
    Proto::StyxSealed,
    Proto::StyxClear,
    Proto::Lethe,
    Proto::LetheClear,
];

/// The configurations of this run, from [`protos`].
static ACTIVE: std::sync::OnceLock<Vec<Proto>> = std::sync::OnceLock::new();

fn active() -> Vec<Proto> {
    ACTIVE.get().cloned().unwrap_or_else(|| ALL[..3].to_vec())
}

/// The configurations this tree can run: all four when `lethed -A`
/// starts, else the first three.
fn protos(ws: &Path, tree: &Path) -> Vec<Proto> {
    let clear = server(ws, tree, Proto::LetheClear, 0, None).is_ok();
    if !clear {
        eprintln!("lethe-perf: lethed has no -A; leaving out lethe-A");
    }
    ALL.into_iter()
        .filter(|p| clear || *p != Proto::LetheClear)
        .collect()
}

impl Proto {
    fn name(self) -> &'static str {
        match self {
            Proto::StyxSealed => "styx-v3",
            Proto::StyxClear => "styx-A",
            Proto::Lethe => "lethe",
            Proto::LetheClear => "lethe-A",
        }
    }

    /// The command that attaches `addr` on the mount point.
    fn attach(self, addr: &str) -> String {
        match self {
            Proto::StyxSealed => format!("mount {addr} {MTPT}"),
            Proto::StyxClear => format!("mount -A {addr} {MTPT}"),
            Proto::Lethe => format!("lethe {addr} {MTPT}"),
            Proto::LetheClear => format!("lethe -A {addr} {MTPT}"),
        }
    }
}

struct Opts {
    reps: usize,
    parts: Vec<String>,
}

fn parse(args: &[String]) -> Option<Opts> {
    let mut o = Opts {
        reps: 5,
        parts: ["direct", "wasmi", "counts", "sweep", "restart"]
            .map(String::from)
            .to_vec(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--reps" => o.reps = it.next()?.parse().ok()?,
            "--parts" => o.parts = it.next()?.split(',').map(String::from).collect(),
            _ => return None,
        }
    }
    Some(o)
}

pub fn cmd(args: &[String]) -> Result<()> {
    let o = parse(args).ok_or(
        "usage: cargo xtask lethe-perf [--reps N] [--parts direct,wasmi,counts,sweep,restart,kernel]",
    )?;
    let ws = workspace();
    super::build_root()?;
    run(cargo()
        .current_dir(&ws)
        .args(["build", "--release", "-p", "purg-emu"]))?;
    let out = ws.join("target/lethe-perf");
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let tree = make_tree(&ws)?;
    let _ = ACTIVE.set(protos(&ws, &tree));
    let mut summary = String::new();
    let _ = writeln!(summary, "# lethe-perf results\n");
    let _ = writeln!(summary, "{}", machine());
    for part in &o.parts {
        let load0 = loadavg();
        let start = Instant::now();
        let text = match part.as_str() {
            "direct" => timing(&ws, &tree, "wasmtime", o.reps, false)?,
            "wasmi" => timing(&ws, &tree, "wasmi", 1, false)?,
            "counts" => timing(&ws, &tree, "wasmtime", 1, true)?,
            "sweep" => sweep(&ws, &tree)?,
            "restart" => restart(&ws, &tree)?,
            "kernel" => kernel(&ws, &tree, o.reps.min(3))?,
            p => return Err(format!("unknown part {p}")),
        };
        let _ = writeln!(
            summary,
            "## {part}\n\nLoad average before {load0}, after {}; took {:.0} s.\n\n{text}",
            loadavg(),
            start.elapsed().as_secs_f64()
        );
        let path = out.join("summary.md");
        fs::write(&path, &summary).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    println!("{summary}");
    Ok(())
}

fn machine() -> String {
    let cpus = thread::available_parallelism().map_or(0, |n| n.get());
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    format!(
        "Host: {} {}, {cpus} CPUs, kernel {}.",
        std::env::consts::OS,
        std::env::consts::ARCH,
        kernel.trim()
    )
}

fn loadavg() -> String {
    fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| String::from("?"))
}

/// Deterministic bytes for the big files.
fn fill(buf: &mut [u8], seed: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let b = seed.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Makes the served tree, unless the stamp says it is there:
///
/// - `one`: 64 bytes; `big1m`, `big100m`: 1 and 100 MiB;
/// - `small/f00000` to `f09999`: 1 KiB each;
/// - `deep/d01/.../d16/leaf`: 1 KiB, 17 elements below `deep`;
/// - `tree/aNN/bNN/fNN`: 10 x 10 directories of 20 files of 100 to 4000
///   bytes, for `du`.
fn make_tree(ws: &Path) -> Result<PathBuf> {
    let top = ws.join("root/tmp/lethe-perf");
    let srv = top.join("srv");
    let stamp = top.join("made");
    mkdir(&top.join("n"))?;
    if fs::read_to_string(&stamp).ok().as_deref() == Some(TREE_STAMP) {
        return Ok(srv);
    }
    eprintln!("lethe-perf: making the tree in {}", srv.display());
    let _ = fs::remove_dir_all(&srv);
    mkdir(&srv.join("small"))?;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut buf = vec![0u8; 1 << 20];
    fill(&mut buf[..64], &mut seed);
    write(&srv.join("one"), &buf[..64])?;
    fill(&mut buf, &mut seed);
    write(&srv.join("big1m"), &buf)?;
    let mut f = fs::File::create(srv.join("big100m")).map_err(|e| format!("big100m: {e}"))?;
    for _ in 0..100 {
        fill(&mut buf, &mut seed);
        f.write_all(&buf).map_err(|e| format!("big100m: {e}"))?;
    }
    for i in 0..SMALL {
        fill(&mut buf[..1024], &mut seed);
        write(&srv.join(format!("small/f{i:05}")), &buf[..1024])?;
    }
    let mut deep = srv.join("deep");
    for i in 1..=16 {
        deep.push(format!("d{i:02}"));
    }
    mkdir(&deep)?;
    write(&deep.join("leaf"), &buf[..1024])?;
    for a in 0..10 {
        for b in 0..10 {
            let d = srv.join(format!("tree/a{a:02}/b{b:02}"));
            mkdir(&d)?;
            for i in 0..20 {
                let n = 100 + (a * 200 + b * 20 + i) * 97 % 3900;
                write(&d.join(format!("f{i:02}")), &buf[..n])?;
            }
        }
    }
    write(&stamp, TREE_STAMP.as_bytes())?;
    Ok(srv)
}

/// Frame and byte totals, each way.
#[derive(Default)]
struct Counts {
    c2s_frames: AtomicU64,
    c2s_bytes: AtomicU64,
    s2c_frames: AtomicU64,
    s2c_bytes: AtomicU64,
    conns: AtomicU64,
}

/// A TCP relay on 127.0.0.1 in front of `127.0.0.1:target`. Each
/// connection is relayed to a fresh connection to the target. Frames are
/// counted by their `size[4]` prefix, which Styx messages, Styx records
/// and Lethe frames all have. Each direction is delayed by half of `rtt`.
/// A connection to `ctl` gets one line of totals.
struct Relay {
    port: u16,
    ctl: u16,
    counts: Arc<Counts>,
}

impl Relay {
    fn start(target: u16, rtt: Duration) -> Result<Relay> {
        let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("relay: {e}"))?;
        let c = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("relay: {e}"))?;
        let port = l.local_addr().map_err(|e| e.to_string())?.port();
        let ctl = c.local_addr().map_err(|e| e.to_string())?.port();
        let counts = Arc::new(Counts::default());
        let k = counts.clone();
        thread::spawn(move || {
            for s in c.incoming() {
                let Ok(mut s) = s else { continue };
                let line = format!(
                    "{} {} {} {}\n",
                    k.c2s_frames.load(Ordering::Acquire),
                    k.c2s_bytes.load(Ordering::Acquire),
                    k.s2c_frames.load(Ordering::Acquire),
                    k.s2c_bytes.load(Ordering::Acquire)
                );
                let _ = s.write_all(line.as_bytes());
            }
        });
        let k = counts.clone();
        thread::spawn(move || {
            for client in l.incoming() {
                let Ok(client) = client else { continue };
                let Ok(server) = TcpStream::connect(("127.0.0.1", target)) else {
                    continue;
                };
                k.conns.fetch_add(1, Ordering::AcqRel);
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                let half = rtt / 2;
                let (c2, s2) = match (client.try_clone(), server.try_clone()) {
                    (Ok(c), Ok(s)) => (c, s),
                    _ => continue,
                };
                let k1 = k.clone();
                let k2 = k.clone();
                thread::spawn(move || pump(client, server, half, &k1, true));
                thread::spawn(move || pump(s2, c2, half, &k2, false));
            }
        });
        Ok(Relay { port, ctl, counts })
    }
}

/// Copies `from` to `to`, counting frames as they arrive and delaying
/// each chunk by `delay` without limiting throughput.
fn pump(mut from: TcpStream, mut to: TcpStream, delay: Duration, k: &Counts, c2s: bool) {
    let (frames, bytes) = if c2s {
        (&k.c2s_frames, &k.c2s_bytes)
    } else {
        (&k.s2c_frames, &k.s2c_bytes)
    };
    let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
    let writer = (!delay.is_zero()).then(|| {
        let mut to = to.try_clone().expect("clone");
        thread::spawn(move || {
            for (due, chunk) in rx {
                let now = Instant::now();
                if due > now {
                    thread::sleep(due - now);
                }
                if to.write_all(&chunk).is_err() {
                    break;
                }
            }
            let _ = to.shutdown(Shutdown::Write);
        })
    });
    let mut frame = FrameCounter::default();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        frames.fetch_add(frame.feed(&buf[..n]), Ordering::AcqRel);
        bytes.fetch_add(n as u64, Ordering::AcqRel);
        if writer.is_some() {
            if tx
                .send((Instant::now() + delay, buf[..n].to_vec()))
                .is_err()
            {
                break;
            }
        } else if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    drop(tx);
    match writer {
        Some(w) => {
            let _ = w.join();
        }
        None => {
            let _ = to.shutdown(Shutdown::Write);
        }
    }
    let _ = from.shutdown(Shutdown::Read);
}

/// Splits a stream into `size[4]`-prefixed frames.
#[derive(Default)]
struct FrameCounter {
    head: [u8; 4],
    have: usize,
    left: u64,
}

impl FrameCounter {
    /// Returns how many frames start in `b`.
    fn feed(&mut self, mut b: &[u8]) -> u64 {
        let mut n = 0;
        while !b.is_empty() {
            if self.left > 0 {
                let k = (self.left as usize).min(b.len());
                self.left -= k as u64;
                b = &b[k..];
                continue;
            }
            self.head[self.have] = b[0];
            self.have += 1;
            b = &b[1..];
            if self.have == 4 {
                self.have = 0;
                n += 1;
                self.left = u64::from(u32::from_le_bytes(self.head)).saturating_sub(4);
            }
        }
        n
    }
}

/// A server process, killed when dropped.
struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the server for `p` on `port` (0 picks one). `ticket` is a
/// ticket key file for `lethed`.
fn server(ws: &Path, tree: &Path, p: Proto, port: u16, ticket: Option<&Path>) -> Result<Server> {
    match p {
        Proto::Lethe | Proto::LetheClear => {
            let mut cmd = Command::new(ws.join("target/release/lethed"));
            cmd.args(["-a", &format!("127.0.0.1:{port}")]);
            if p == Proto::Lethe {
                cmd.arg("-k").arg(ws.join("root/lib/test/auth.key.pub"));
            } else {
                cmd.arg("-A");
            }
            if let Some(t) = ticket {
                cmd.arg("-t").arg(t);
            }
            let mut child = cmd
                .arg(tree)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("lethed: {e}"))?;
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("stdout"))
                .read_line(&mut line)
                .map_err(|e| format!("lethed: {e}"))?;
            let port = line
                .trim()
                .rsplit(':')
                .next()
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("lethed said {line:?}"))?;
            Ok(Server { child, port })
        }
        Proto::StyxSealed | Proto::StyxClear => {
            let flag = if p == Proto::StyxSealed { "-e" } else { "-A" };
            let script = format!(
                "bind /lib/test/keys /lib/keys; styxlisten tcp!127.0.0.1!{port} export {flag} -r {GUEST_TREE}"
            );
            let mut child = Command::new(ws.join("target/release/emu"))
                .arg("-r")
                .arg(ws.join("root"))
                .args(["sh", "-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("emu: {e}"))?;
            let out = child.stdout.take().expect("stdout");
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    if let Some(a) = line.strip_prefix("styxlisten: ") {
                        let _ = tx.send(a.to_owned());
                    } else {
                        eprintln!("export emu: {line}");
                    }
                }
            });
            let mut s = Server { child, port: 0 };
            let addr = rx
                .recv_timeout(Duration::from_secs(30))
                .map_err(|_| String::from("the export emu did not announce"))?;
            s.port = addr
                .rsplit('!')
                .next()
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("styxlisten said {addr:?}"))?;
            Ok(s)
        }
    }
}

/// The client script: `reps` times, attach, run fsperf, detach.
fn script(p: Proto, addr: &str, fsperf: &str, reps: usize) -> String {
    let mut s = format!("mkdir -p {MTPT}\n");
    for _ in 0..reps {
        let _ = writeln!(
            s,
            "{} || echo attach failed\n/bin/test/fsperf.wasm {fsperf}\nunmount {MTPT}",
            p.attach(addr)
        );
    }
    s
}

/// Runs `script` in a client emu; returns its standard output.
fn client(ws: &Path, vm: &str, script: &str) -> Result<String> {
    let o = Command::new(ws.join("target/release/emu"))
        .arg("-r")
        .arg(ws.join("root"))
        .arg(format!("--vm={vm}"))
        .args(["sh", "-c", script])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("emu: {e}"))?;
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// One measured run of a case: time, operations, counts, extra fields.
#[derive(Clone)]
struct Sample {
    ms: f64,
    ops: f64,
    counts: Option<[u64; 4]>,
    extra: Vec<String>,
}

type Results = BTreeMap<(String, String), Vec<Sample>>;

/// Parses fsperf's `R` lines; `E` lines and anything else are reported.
fn parse_results(out: &str, label: &str) -> Results {
    let mut r = Results::new();
    for line in out.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.first() == Some(&"R") && w.len() >= 9 {
            let counts = (w[5] != "-").then(|| {
                let n = |i: usize| w[i].parse().unwrap_or(0);
                [n(5), n(6), n(7), n(8)]
            });
            r.entry((w[1].to_owned(), w[2].to_owned()))
                .or_default()
                .push(Sample {
                    ms: w[3].parse().unwrap_or(f64::NAN),
                    ops: w[4].parse().unwrap_or(1.0),
                    counts,
                    extra: w[9..].iter().map(|s| s.to_string()).collect(),
                });
        } else if !line.trim().is_empty() {
            eprintln!("lethe-perf: {label}: {line}");
        }
    }
    r
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

/// `median (min-max)` of the times, with a per-operation figure: MiB/s for
/// the big reads, the latency cases' own median, else microseconds per
/// operation.
fn cell(case: &str, s: &[Sample]) -> String {
    let mut ms: Vec<f64> = s.iter().map(|x| x.ms).collect();
    let med = median(&mut ms);
    let (lo, hi) = (ms[0], ms[ms.len() - 1]);
    let ops = s[0].ops.max(1.0);
    let per = match case {
        "read1m" => format!("{:.0} MiB/s", 1000.0 / med),
        "read100m" => format!("{:.0} MiB/s", 100.0 * 1000.0 / med),
        "lat" | "latread" => {
            let mut m: Vec<f64> = s
                .iter()
                .filter_map(|x| x.extra.first()?.parse().ok())
                .collect();
            format!("{:.0} us/op", median(&mut m))
        }
        _ => format!("{:.1} us/op", med * 1000.0 / ops),
    };
    if s.len() > 1 {
        format!("{med:.1} ({lo:.1}-{hi:.1}) ms, {per}")
    } else {
        format!("{med:.1} ms, {per}")
    }
}

/// A table of times: a row per case and phase, a column per protocol.
fn time_table(all: &BTreeMap<Proto, Results>, cases: &[&str]) -> String {
    let mut t = String::from("| case | phase |");
    for p in all.keys() {
        let _ = write!(t, " {} |", p.name());
    }
    t.push_str("\n|---|---|");
    for _ in all.keys() {
        t.push_str("---|");
    }
    t.push('\n');
    for case in cases {
        for phase in ["cold", "warm"] {
            let _ = write!(t, "| {case} | {phase} |");
            for r in all.values() {
                match r.get(&(case.to_string(), phase.to_string())) {
                    Some(s) if !s.is_empty() => {
                        let _ = write!(t, " {} |", cell(case, s));
                    }
                    _ => t.push_str(" - |"),
                }
            }
            t.push('\n');
        }
    }
    t
}

/// A table of frames and bytes each way, per case and phase.
fn count_table(all: &BTreeMap<Proto, Results>, cases: &[&str]) -> String {
    let mut t = String::from(
        "| case | phase | proto | ops | c2s frames | c2s bytes | s2c frames | s2c bytes | round trips/op | bytes/op |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for case in cases {
        for phase in ["cold", "warm"] {
            for (p, r) in all {
                let Some(s) = r.get(&(case.to_string(), phase.to_string())) else {
                    continue;
                };
                let Some(c) = s[0].counts else { continue };
                let ops = s[0].ops.max(1.0);
                let _ = writeln!(
                    t,
                    "| {case} | {phase} | {} | {} | {} | {} | {} | {} | {:.2} | {:.0} |",
                    p.name(),
                    s[0].ops,
                    c[0],
                    c[1],
                    c[2],
                    c[3],
                    c[0] as f64 / ops,
                    (c[1] + c[3]) as f64 / ops
                );
            }
        }
    }
    t
}

/// The fsperf arguments for a run.
fn fsperf_args(ctl: Option<String>, files: usize, reps: usize, cases: &[&str]) -> String {
    let ctl = ctl.map(|c| format!("-c {c} ")).unwrap_or_default();
    format!("{ctl}-n {files} -k {reps} {MTPT} {}", cases.join(" "))
}

/// Runs the full cases `reps` times per protocol on `vm`, directly or
/// through a relay that counts (with no added delay).
fn timing(ws: &Path, tree: &Path, vm: &str, reps: usize, counted: bool) -> Result<String> {
    let out = ws.join("target/lethe-perf");
    let mut all = BTreeMap::new();
    for p in active() {
        let srv = server(ws, tree, p, 0, None)?;
        let relay = if counted {
            Some(Relay::start(srv.port, Duration::ZERO)?)
        } else {
            None
        };
        let port = relay.as_ref().map_or(srv.port, |r| r.port);
        let ctl = relay.as_ref().map(|r| format!("tcp!127.0.0.1!{}", r.ctl));
        let args = fsperf_args(ctl, SMALL, 200, CASES);
        let addr = format!("tcp!127.0.0.1!{port}");
        eprintln!("lethe-perf: {vm} {} x{reps}", p.name());
        let text = client(ws, vm, &script(p, &addr, &args, reps))?;
        let tag = format!(
            "{}-{vm}-{}",
            if counted { "counts" } else { "time" },
            p.name()
        );
        let _ = fs::write(out.join(format!("{tag}.txt")), &text);
        all.insert(p, parse_results(&text, &tag));
        if let Some(r) = relay {
            eprintln!(
                "lethe-perf: {}: {} connections",
                p.name(),
                r.counts.conns.load(Ordering::Acquire)
            );
        }
    }
    Ok(if counted {
        format!(
            "Through the counting relay, one run, {vm}:\n\n{}\n{}",
            count_table(&all, CASES),
            time_table(&all, CASES)
        )
    } else {
        format!("{vm}, {reps} runs:\n\n{}", time_table(&all, CASES))
    })
}

/// The latency sweep: the smaller cases through a relay at each RTT.
fn sweep(ws: &Path, tree: &Path) -> Result<String> {
    let out = ws.join("target/lethe-perf");
    let mut text = String::new();
    // One table per case: a row per RTT, a column per protocol.
    let mut by: BTreeMap<u64, BTreeMap<Proto, Results>> = BTreeMap::new();
    for &rtt in SWEEP_RTT_MS {
        for p in active() {
            let srv = server(ws, tree, p, 0, None)?;
            let relay = Relay::start(srv.port, Duration::from_millis(rtt))?;
            let ctl = format!("tcp!127.0.0.1!{}", relay.ctl);
            let args = fsperf_args(Some(ctl), 20, 20, SWEEP_CASES);
            let addr = format!("tcp!127.0.0.1!{}", relay.port);
            eprintln!("lethe-perf: sweep {rtt} ms {}", p.name());
            let o = client(ws, "wasmtime", &script(p, &addr, &args, 1))?;
            let tag = format!("sweep-{rtt}ms-{}", p.name());
            let _ = fs::write(out.join(format!("{tag}.txt")), &o);
            by.entry(rtt)
                .or_default()
                .insert(p, parse_results(&o, &tag));
        }
    }
    let _ = writeln!(
        text,
        "20 files, 20 repetitions, wasmtime; cold runs, time in ms (round trips client to server in brackets).\n"
    );
    let _ = write!(text, "| case | RTT |");
    for p in active() {
        let _ = write!(text, " {} |", p.name());
    }
    text.push_str("\n|---|---:|");
    for _ in active() {
        text.push_str("---:|");
    }
    text.push('\n');
    for case in SWEEP_CASES {
        for (rtt, all) in &by {
            let _ = write!(text, "| {case} | {rtt} ms |");
            for p in active() {
                let s = all
                    .get(&p)
                    .and_then(|r| r.get(&(case.to_string(), String::from("cold"))));
                match s {
                    Some(s) => {
                        let rt = s[0].counts.map_or(0, |c| c[0]);
                        let _ = write!(text, " {:.0} ({rt}) |", s[0].ms);
                    }
                    None => text.push_str(" - |"),
                }
            }
            text.push('\n');
        }
    }
    let _ = writeln!(text, "\nWarm runs:\n");
    for (rtt, all) in &by {
        let _ = writeln!(text, "RTT {rtt} ms:\n\n{}", time_table(all, SWEEP_CASES));
    }
    Ok(text)
}

/// Restarts each server in the middle of a read through a 1 ms relay and
/// records what the client saw.
fn restart(ws: &Path, tree: &Path) -> Result<String> {
    let out = ws.join("target/lethe-perf");
    let key = out.join("ticket.key");
    write(&key, &[0x5a; 32])?;
    let mut text = String::new();
    let cases: [(Proto, Option<&Path>, &str); 4] = [
        (Proto::StyxSealed, None, "styx-v3"),
        (Proto::StyxClear, None, "styx-A"),
        (Proto::Lethe, None, "lethe, fresh ticket key"),
        (Proto::Lethe, Some(&key), "lethe -t, same ticket key"),
    ];
    for (p, ticket, label) in cases {
        let port = free_port()?;
        let srv = server(ws, tree, p, port, ticket)?;
        let relay = Relay::start(port, Duration::from_millis(1))?;
        let addr = format!("tcp!127.0.0.1!{}", relay.port);
        let script = format!(
            "mkdir -p {MTPT}\n{} || echo attach failed\n/bin/test/fsperf.wasm {MTPT} restart\nls {MTPT}/one\nunmount {MTPT}\nsleep 3\necho attach again\n{} && ls {MTPT}/one && unmount {MTPT}",
            p.attach(&addr),
            p.attach(&addr)
        );
        let mut child = Command::new(ws.join("target/release/emu"))
            .arg("-r")
            .arg(ws.join("root"))
            .args(["sh", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("emu: {e}"))?;
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let (tx, rx) = mpsc::channel();
        let readers: Vec<_> = [
            Box::new(child.stdout.take().expect("stdout")) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().expect("stderr")),
        ]
        .into_iter()
        .map(|r| {
            let lines = lines.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(r).lines() {
                    let Ok(line) = line else { break };
                    if line == "reading" {
                        let _ = tx.send(());
                    }
                    lines.lock().expect("lines").push(line);
                }
            })
        })
        .collect();
        rx.recv_timeout(Duration::from_secs(60))
            .map_err(|_| format!("{label}: the client never started reading"))?;
        thread::sleep(Duration::from_secs(2));
        drop(srv);
        let down = Instant::now();
        thread::sleep(Duration::from_secs(1));
        let srv = server(ws, tree, p, port, ticket)?;
        let up = down.elapsed();
        let status = child.wait().map_err(|e| e.to_string())?;
        for r in readers {
            let _ = r.join();
        }
        drop(srv);
        let lines = lines.lock().expect("lines").join("\n");
        let _ = fs::write(out.join(format!("restart-{}.txt", p.name())), &lines);
        let _ = writeln!(
            text,
            "### {label}\n\nServer down for {:.1} s, {} connections through the relay; client exit {status}.\n\n```\n{lines}\n```\n",
            up.as_secs_f64(),
            relay.counts.conns.load(Ordering::Acquire)
        );
    }
    Ok(text)
}

/// Builds the kernel and boots it under KVM with the initrd, a virtio-net
/// card on user networking and a shared directory holding the script,
/// which it types on the console.
fn kernel(ws: &Path, tree: &Path, reps: usize) -> Result<String> {
    if std::env::consts::ARCH != "aarch64" || !Path::new("/dev/kvm").exists() {
        return Ok(String::from(
            "Skipped: needs an aarch64 host with /dev/kvm.\n",
        ));
    }
    let out = ws.join("target/lethe-perf");
    let initrd = initrd()?;
    run(crate::align::image_cargo(ws, false)?.args([
        "build",
        "-p",
        "purg-kernel",
        "--target",
        PROBE_TARGET,
        "--profile",
        "kernel",
    ]))?;
    let elf_path = ws
        .join("target")
        .join(PROBE_TARGET)
        .join("kernel/purg-kernel");
    let elf = fs::read(&elf_path).map_err(|e| format!("{}: {e}", elf_path.display()))?;
    let image = out.join("purg-kernel.img");
    write(&image, &elf_to_image(&elf)?)?;
    let share = out.join("share");
    mkdir(&share)?;
    let mut servers = Vec::new();
    let mut sh = format!("mkdir -p {MTPT}\n");
    let mut relays = Vec::new();
    for p in active() {
        let srv = server(ws, tree, p, 0, None)?;
        let relay = Relay::start(srv.port, Duration::ZERO)?;
        let ctl = format!("tcp!{KERNEL_HOST}!{}", relay.ctl);
        let addr = format!("tcp!{KERNEL_HOST}!{}", relay.port);
        let args = fsperf_args(Some(ctl), SMALL, 200, CASES);
        let _ = writeln!(sh, "echo proto {}", p.name());
        sh.push_str(&script(p, &addr, &args, reps));
        servers.push(srv);
        relays.push(relay);
    }
    sh.push_str("echo perf done\n");
    write(&share.join("perf.sh"), sh.as_bytes())?;
    let share_s = share.to_str().ok_or("share path is not UTF-8")?;
    let initrd_s = initrd.to_str().ok_or("initrd path is not UTF-8")?;
    let fsdev = format!("local,id=host,path={share_s},security_model=none");
    let netdev_mac = format!("virtio-net-device,netdev=net0,mac={GUEST_MAC}");
    let args = [
        "-accel",
        "kvm",
        "-cpu",
        "host",
        "-M",
        "virt,gic-version=3",
        "-m",
        "1G",
        "-nographic",
        "-initrd",
        initrd_s,
        "-append",
        "exit=psci",
        "-netdev",
        "user,id=net0",
        "-global",
        "virtio-mmio.force-legacy=off",
        "-device",
        &netdev_mac,
        "-fsdev",
        &fsdev,
        "-device",
        "virtio-9p-device,fsdev=host,mount_tag=host",
        "-kernel",
    ];
    let qemu = std::env::var("QEMU").unwrap_or_else(|_| String::from("qemu-system-aarch64"));
    let mut child = Command::new(&qemu)
        .args(args)
        .arg(&image)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{qemu}: {e}"))?;
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    let serial = Arc::new(Mutex::new(Vec::<u8>::new()));
    let s2 = serial.clone();
    let reader = thread::spawn(move || {
        let mut b = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut b) {
            if n == 0 {
                break;
            }
            s2.lock().expect("serial").extend_from_slice(&b[..n]);
        }
    });
    let text = || String::from_utf8_lossy(&serial.lock().expect("serial")).replace('\r', "");
    let start = Instant::now();
    let mut typed = false;
    let limit = Duration::from_secs(3600);
    loop {
        let now = text();
        if !typed && now.contains("kernel: root ") && now.ends_with("; ") {
            thread::sleep(Duration::from_millis(500));
            stdin
                .write_all(b"sh /n/host/perf.sh\r")
                .map_err(|e| format!("serial: {e}"))?;
            typed = true;
        }
        if now.contains("\nperf done\n") || start.elapsed() > limit {
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    drop(stdin);
    let serial = text();
    let _ = fs::write(out.join("kernel-serial.txt"), &serial);
    if !serial.contains("\nperf done\n") {
        return Err(format!(
            "the kernel run did not finish; serial in {}",
            out.join("kernel-serial.txt").display()
        ));
    }
    // Split the serial output by protocol.
    let mut all = BTreeMap::new();
    let mut cur: Option<Proto> = None;
    let mut chunks: BTreeMap<Proto, String> = BTreeMap::new();
    for line in serial.lines() {
        if let Some(n) = line.strip_prefix("proto ") {
            cur = active().into_iter().find(|p| p.name() == n.trim());
            continue;
        }
        if let Some(p) = cur {
            let c = chunks.entry(p).or_default();
            c.push_str(line);
            c.push('\n');
        }
    }
    for (p, c) in chunks {
        all.insert(p, parse_results(&c, &format!("kernel {}", p.name())));
    }
    drop(relays);
    drop(servers);
    Ok(format!(
        "Kernel under KVM (wasmi), QEMU user networking, through the counting relay, {reps} runs:\n\n{}\n{}",
        time_table(&all, CASES),
        count_table(&all, CASES)
    ))
}
