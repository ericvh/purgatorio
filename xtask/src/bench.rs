//! `cargo xtask bench`: the benchmark suite of issue #27 (`docs/perf.md`).
//!
//! The suite is one sh script, `root/lib/bench/bench.sh`, run inside the
//! system: on the emu with each engine, and on the kernel under KVM or
//! TCG. Every result it prints is a line `bench NAME ...`; this collects
//! them, runs each configuration's rounds (and boots) again, and prints
//! the median and spread of each benchmark per configuration, then
//! writes every sample to `target/bench/results.json`.
//!
//! The times are taken inside the system (`time`, and the `bench` guest's
//! clock), so a kernel's boot is not in them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::{
    PROBE_TARGET, Result, align, aot_initrd, cargo, elf_to_image, initrd, run, run_qemu, workspace,
};

const USAGE: &str = "usage: cargo xtask bench [--emu] [--kernel] [--kvm] [--tcg] [--vm=wasmtime,wasmi] \
[--rounds N] [--boots N] [--features F,...] [--profile P] [--label L] [--strict-align] [--rustflags FLAGS] [--config K=V] [--aot native|pulley] [--aot-files native|pulley] [--kprof] [--out FILE]";

/// The size of the kernel's disk image: room for the 100 MiB file, the
/// file system's own blocks and the 1,000 small files `bench fs` makes.
const DISK_SIZE: u64 = 160 << 20;
/// The file `cat-disk` reads, as `bench.sh` makes it in `/tmp`.
const BIG: usize = 100 << 20;
/// A limit against hangs, not a target. A TCG round on wasmi takes a few
/// minutes.
const RUN_TIMEOUT: Duration = Duration::from_secs(3600);
/// QEMU's `-m`: the 100 MiB file is in the RAM tree, and read whole.
const KERNEL_MEM: &str = "1G";

struct Opts {
    emu: bool,
    kernel: bool,
    kvm: bool,
    tcg: bool,
    vms: Vec<String>,
    rounds: usize,
    boots: usize,
    features: Vec<String>,
    profile: String,
    label: Option<String>,
    /// Build the kernel with strict alignment throughout, as the target
    /// defines it, rather than in the boot path only (`xtask/src/align.rs`).
    strict_align: bool,
    /// Extra rustflags for the kernel build.
    rustflags: Option<String>,
    /// `--config` settings for the kernel build (profile overrides).
    config: Vec<String>,
    /// Build the kernel with the `aot` feature and images of this kind
    /// (`native` or `pulley`) for [`AOT_MODULES`].
    aot: Option<String>,
    /// Build the kernel with the `aot` feature and no linked images, and
    /// boot it on an initrd with a signed image file of this kind beside
    /// every module (`xtask kernel --aot`).
    aot_files: Option<String>,
    /// Boot the kernel with its profiler and report where the profiled
    /// benchmarks spent their time (`crate::kprof`).
    kprof: bool,
    out: Option<PathBuf>,
}

/// The benchmarks `bench.sh` profiles under `--kprof`, as it names them.
const PROFILED: &[&str] = &[
    "cat-root",
    "cat-styx",
    "cat-styx-sealed",
    "cat-disk",
    "sha1-file-16m",
    "fs-disk-1k",
];

/// Samples a second under `--kprof`.
const KPROF_HZ: u32 = 2000;

impl Opts {
    fn parse(args: &[String]) -> Result<Opts> {
        let mut o = Opts {
            emu: false,
            kernel: false,
            kvm: false,
            tcg: false,
            vms: vec![String::from("wasmtime"), String::from("wasmi")],
            rounds: 3,
            boots: 1,
            features: Vec::new(),
            profile: String::from("kernel"),
            label: None,
            strict_align: false,
            rustflags: None,
            config: Vec::new(),
            aot: None,
            aot_files: None,
            kprof: false,
            out: None,
        };
        let mut it = args.iter();
        let num = |v: Option<&String>| -> Result<usize> {
            v.and_then(|s| s.parse().ok())
                .filter(|&n| n > 0)
                .ok_or_else(|| String::from(USAGE))
        };
        while let Some(a) = it.next() {
            match a.as_str() {
                "--emu" => o.emu = true,
                "--kernel" => o.kernel = true,
                "--kvm" => o.kvm = true,
                "--tcg" => o.tcg = true,
                "--kprof" => o.kprof = true,
                "--rounds" => o.rounds = num(it.next())?,
                "--boots" => o.boots = num(it.next())?,
                "--features" => {
                    let f = it.next().ok_or(USAGE)?;
                    o.features = f.split(',').map(String::from).collect();
                }
                "--profile" => o.profile = it.next().ok_or(USAGE)?.clone(),
                "--aot" => {
                    let k = it.next().ok_or(USAGE)?;
                    if k != "native" && k != "pulley" {
                        return Err(String::from(USAGE));
                    }
                    o.aot = Some(k.clone());
                }
                "--aot-files" => {
                    let k = it.next().ok_or(USAGE)?;
                    if k != "native" && k != "pulley" {
                        return Err(String::from(USAGE));
                    }
                    o.aot_files = Some(k.clone());
                }
                "--config" => o.config.push(it.next().ok_or(USAGE)?.clone()),
                "--strict-align" => o.strict_align = true,
                "--rustflags" => o.rustflags = Some(it.next().ok_or(USAGE)?.clone()),
                "--label" => o.label = Some(it.next().ok_or(USAGE)?.clone()),
                "--out" => o.out = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
                _ => match a.strip_prefix("--vm=") {
                    Some(v) => o.vms = v.split(',').map(String::from).collect(),
                    None => return Err(String::from(USAGE)),
                },
            }
        }
        // `--kvm` or `--tcg` alone means the kernel only.
        if o.kvm || o.tcg {
            o.kernel = true;
        }
        if !o.emu && !o.kernel {
            o.emu = true;
            o.kernel = true;
        }
        if o.kernel && !o.kvm && !o.tcg {
            if kvm_usable() {
                o.kvm = true;
            } else {
                o.tcg = true;
            }
        }
        Ok(o)
    }
}

fn kvm_usable() -> bool {
    std::env::consts::ARCH == "aarch64" && Path::new("/dev/kvm").exists()
}

/// One configuration's samples, by benchmark, in the order first seen.
#[derive(Default)]
struct Samples {
    order: Vec<String>,
    by_name: BTreeMap<String, Vec<f64>>,
    load: Vec<String>,
}

impl Samples {
    fn add(&mut self, name: &str, v: f64) {
        if !self.by_name.contains_key(name) {
            self.order.push(String::from(name));
        }
        self.by_name.entry(String::from(name)).or_default().push(v);
    }

    /// Takes the `bench NAME ...` lines of one run. A line is either
    /// `bench NAME SECONDS` or `bench NAME` and `time`'s `Nl Nr Nt`, of
    /// which the real time counts. Anything else on the line (a failed
    /// command's message) makes it an error.
    fn parse(&mut self, out: &str) -> Result<usize> {
        let mut n = 0;
        let mut started = false;
        for line in out.lines() {
            let line = line.trim_end_matches('\r');
            if line == "bench-start" {
                started = true;
            }
            let Some(rest) = line.strip_prefix("bench ") else {
                continue;
            };
            let mut f = rest.split_whitespace();
            let name = f.next().ok_or_else(|| format!("bad line {line:?}"))?;
            let fields: Vec<&str> = f.collect();
            let v = match fields.as_slice() {
                [s] => s.parse::<f64>().ok(),
                [_, r, _] => r.strip_suffix('r').and_then(|r| r.parse().ok()),
                _ => None,
            }
            .ok_or_else(|| format!("benchmark failed: {line:?}"))?;
            self.add(name, v);
            n += 1;
        }
        if !started || !out.contains("bench-done") {
            return Err(String::from("the suite did not finish"));
        }
        Ok(n)
    }
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    if s.len() % 2 == 1 {
        s[m]
    } else {
        (s[m - 1] + s[m]) / 2.0
    }
}

fn min_max(v: &[f64]) -> (f64, f64) {
    v.iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &x| {
            (lo.min(x), hi.max(x))
        })
}

/// The 1-, 5- and 15-minute load averages, where the host has them.
fn loadavg() -> String {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| String::from("unknown"))
}

fn round_args(rounds: usize) -> Vec<String> {
    (1..=rounds).map(|r| r.to_string()).collect()
}

fn run_emu(ws: &Path, vm: &str, rounds: usize, s: &mut Samples) -> Result<()> {
    let emu = ws.join("target/release/emu");
    let t = Instant::now();
    let out = Command::new(&emu)
        .arg("-r")
        .arg(ws.join("root"))
        .arg(format!("--vm={vm}"))
        // The script's status, as the emu's.
        .arg("-x")
        .args(["sh", "/lib/bench/bench.sh"])
        .args(round_args(rounds))
        .output()
        .map_err(|e| format!("{}: {e}", emu.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        eprint!("{text}{}", String::from_utf8_lossy(&out.stderr));
        return Err(format!("emu --vm={vm}: {}", out.status));
    }
    let n = s.parse(&text).inspect_err(|_| eprint!("{text}"))?;
    eprintln!(
        "bench: emu {vm}: {n} results in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The disk the kernel's `cat-disk` and `fs-disk` use: purgfs holding
/// `big` and `rounds`, which `bench.sh` reads for its rounds.
fn kernel_disk(ws: &Path, rounds: usize) -> Result<PathBuf> {
    let dir = ws.join("target/bench/disk");
    let src = dir.join("src");
    let io = |e: std::io::Error| format!("{}: {e}", dir.display());
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(io)?;
    }
    fs::create_dir_all(&src).map_err(io)?;
    fs::write(src.join("big"), vec![0u8; BIG]).map_err(io)?;
    fs::write(src.join("rounds"), round_args(rounds).join(" ") + "\n").map_err(io)?;
    let img = dir.join("disk.img");
    super::disk::build(&img, Some(&src), DISK_SIZE, purg_fs::layout::BSIZE)?;
    fs::remove_dir_all(&src).map_err(io)?;
    Ok(img)
}

/// The modules `--aot` compiles ahead of time: those the suite times.
/// Everything else runs on wasmi.
const AOT_MODULES: &[&str] = &[
    "sh",
    "cat",
    "echo",
    "sha1sum",
    "zeros",
    "time",
    "memfs",
    "export",
    "test/bench",
];

/// Builds `cwasm` and the image set for `--aot`; returns its path.
fn aot_images(ws: &Path, kind: &str) -> Result<PathBuf> {
    run(cargo().current_dir(ws).args([
        "build",
        "--release",
        "-p",
        "purg-cwasm",
        "--bin",
        "cwasm",
    ]))?;
    let set = ws.join(format!("target/bench/aot-{kind}.bin"));
    let mut cmd = Command::new(ws.join("target/release/cwasm"));
    cmd.args(["-k", kind, "-o"]).arg(&set);
    for m in AOT_MODULES {
        cmd.arg(ws.join(format!("root/bin/{m}.wasm")));
    }
    run(&mut cmd)?;
    Ok(set)
}

fn build_kernel(ws: &Path, o: &Opts) -> Result<PathBuf> {
    let mut features = o.features.clone();
    let mut cmd = align::image_cargo_with(ws, o.strict_align, o.rustflags.as_deref())?;
    if let Some(kind) = &o.aot {
        features.push(String::from("aot"));
        cmd.env("PURG_AOT_IMAGES", aot_images(ws, kind)?);
    } else if o.aot_files.is_some() {
        features.push(String::from("aot"));
        cmd.env_remove("PURG_AOT_IMAGES");
    }
    cmd.args([
        "build",
        "-p",
        "purg-kernel",
        "--target",
        PROBE_TARGET,
        "--profile",
        &o.profile,
    ]);
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }
    for c in &o.config {
        cmd.args(["--config", c]);
    }
    // Other flags build in a directory of their own, so as not to
    // rebuild the usual kernel's dependencies over them.
    let mut target = ws.join("target");
    if o.rustflags.is_some() || !o.config.is_empty() || o.strict_align {
        target = ws
            .join("target/bench/build")
            .join(o.label.as_deref().unwrap_or("custom"));
        cmd.arg("--target-dir").arg(&target);
    }
    run(&mut cmd)?;
    let elf = target
        .join(PROBE_TARGET)
        .join(&o.profile)
        .join("purg-kernel");
    if !o.strict_align {
        align::check_image(ws, &elf)?;
    }
    let bytes = fs::read(&elf).map_err(|e| format!("{}: {e}", elf.display()))?;
    let image = elf.with_extension("img");
    fs::write(&image, elf_to_image(&bytes)?).map_err(|e| format!("{}: {e}", image.display()))?;
    Ok(image)
}

fn run_kernel(
    image: &Path,
    initrd: &str,
    disk: &Path,
    (kvm, kprof): (bool, bool),
    s: &mut Samples,
) -> Result<()> {
    let disk = disk.to_str().ok_or("disk path is not UTF-8")?;
    let drive = format!("file={disk},if=none,id=disk,format=raw,cache=unsafe");
    let mut append = String::from(if kvm { "exit=psci bench" } else { "bench" });
    if kprof {
        let _ = write!(append, " kprof={KPROF_HZ}");
    }
    let mut args: Vec<&str> = if kvm {
        vec!["-accel", "kvm", "-cpu", "host", "-M", "virt,gic-version=3"]
    } else {
        vec![
            "-M",
            "virt,gic-version=3",
            "-cpu",
            "cortex-a72",
            "-semihosting",
        ]
    };
    args.extend([
        "-m",
        KERNEL_MEM,
        "-nographic",
        "-initrd",
        initrd,
        "-append",
        &append,
        "-global",
        "virtio-mmio.force-legacy=off",
        "-drive",
        &drive,
        "-device",
        "virtio-blk-device,drive=disk,serial=disk",
        "-kernel",
    ]);
    let which = if kvm { "kvm" } else { "tcg" };
    eprintln!(
        "bench: kernel {which}: qemu-system-aarch64 {}",
        args.join(" ")
    );
    let r = run_qemu(&args, image, RUN_TIMEOUT, &[], |_| Ok(()))?.finished("bench")?;
    if !r.status.success() || r.serial.contains("kernel: bench: ") {
        eprint!("{}", r.serial);
        return Err(format!("kernel {which}: QEMU exited with {}", r.status));
    }
    let n = s
        .parse(&r.serial)
        .inspect_err(|_| eprint!("{}", r.serial))?;
    eprintln!(
        "bench: kernel {which}: {n} results in {:.1}s",
        r.elapsed.as_secs_f64()
    );
    Ok(())
}

/// Prints the table: one row per benchmark, one column per
/// configuration, each cell the median time in seconds, the fastest in
/// brackets, and the spread (the range as a share of the median).
fn table(configs: &[(String, Samples)]) -> String {
    let mut names: Vec<String> = Vec::new();
    for (_, s) in configs {
        for n in &s.order {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }
    let width = names.iter().map(String::len).max().unwrap_or(4).max(9);
    let mut out = String::new();
    let _ = write!(out, "{:width$}", "benchmark");
    for (c, _) in configs {
        let _ = write!(out, "  {c:>24}");
    }
    out.push('\n');
    let samples = configs
        .iter()
        .map(|(_, s)| s.by_name.values().map(Vec::len).max().unwrap_or(0));
    let _ = write!(out, "{:width$}", "(samples)");
    for n in samples {
        let _ = write!(out, "  {n:>24}");
    }
    out.push('\n');
    for n in &names {
        let _ = write!(out, "{n:width$}");
        for (_, s) in configs {
            let cell = match s.by_name.get(n) {
                Some(v) => {
                    let m = median(v);
                    let (lo, hi) = min_max(v);
                    let spread = if m > 0.0 { (hi - lo) / m * 100.0 } else { 0.0 };
                    format!("{m:.3} ({lo:.3}) ±{spread:.0}%")
                }
                None => String::from("-"),
            };
            let _ = write!(out, "  {cell:>24}");
        }
        out.push('\n');
    }
    out
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json(configs: &[(String, Samples)]) -> String {
    let mut o = String::from("{\n  \"configs\": {\n");
    for (i, (c, s)) in configs.iter().enumerate() {
        let _ = writeln!(o, "    {}: {{", json_str(c));
        let loads: Vec<String> = s.load.iter().map(|l| json_str(l)).collect();
        let _ = writeln!(o, "      \"loadavg\": [{}],", loads.join(", "));
        o.push_str("      \"benchmarks\": {\n");
        for (j, n) in s.order.iter().enumerate() {
            let v = &s.by_name[n];
            let (lo, hi) = min_max(v);
            let samples: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
            let _ = write!(
                o,
                "        {}: {{\"median\": {}, \"min\": {lo}, \"max\": {hi}, \"samples\": [{}]}}",
                json_str(n),
                median(v),
                samples.join(", ")
            );
            o.push_str(if j + 1 < s.order.len() { ",\n" } else { "\n" });
        }
        o.push_str("      }\n    }");
        o.push_str(if i + 1 < configs.len() { ",\n" } else { "\n" });
    }
    o.push_str("  }\n}\n");
    o
}

/// `xtask bench`.
pub fn cmd(args: &[String]) -> Result<()> {
    let o = Opts::parse(args)?;
    let ws = workspace();
    let initrd = match &o.aot_files {
        Some(kind) => aot_initrd(&ws, kind)?,
        None => initrd()?,
    };
    let initrd = initrd
        .to_str()
        .ok_or("initrd path is not UTF-8")?
        .to_owned();
    let suffix = o.label.as_ref().map_or(String::new(), |l| format!("+{l}"));
    let mut configs: Vec<(String, Samples)> = Vec::new();
    if o.emu {
        run(cargo()
            .current_dir(&ws)
            .args(["build", "--release", "-p", "purg-emu"]))?;
        for vm in &o.vms {
            let mut s = Samples::default();
            for _ in 0..o.boots {
                s.load.push(loadavg());
                run_emu(&ws, vm, o.rounds, &mut s)?;
            }
            s.load.push(loadavg());
            configs.push((format!("emu-{vm}{suffix}"), s));
        }
    }
    if o.kernel {
        let image = build_kernel(&ws, &o)?;
        let disk = kernel_disk(&ws, o.rounds)?;
        for (on, kvm) in [(o.kvm, true), (o.tcg, false)] {
            if !on {
                continue;
            }
            if kvm && !kvm_usable() {
                eprintln!("bench: skipping KVM: needs an aarch64 host with /dev/kvm");
                continue;
            }
            let mut s = Samples::default();
            let mut profiles = crate::kprof::Profiles::default();
            let names: Vec<String> = PROFILED.iter().map(|n| String::from(*n)).collect();
            for _ in 0..o.boots {
                s.load.push(loadavg());
                run_kernel(&image, &initrd, &disk, (kvm, o.kprof), &mut s)?;
                if o.kprof {
                    profiles.collect(&disk, &names, &round_args(o.rounds))?;
                }
            }
            s.load.push(loadavg());
            let which = if kvm { "kvm" } else { "tcg" };
            if o.kprof {
                let elf = fs::read(image.with_extension(""))
                    .map_err(|e| format!("{}: {e}", image.display()))?;
                let report = profiles.report(&elf)?;
                let path = ws.join(format!("target/bench/kprof-{which}{suffix}.txt"));
                fs::write(&path, &report).map_err(|e| format!("{}: {e}", path.display()))?;
                print!("{report}");
                eprintln!("bench: wrote {}", path.display());
            }
            configs.push((format!("kernel-{which}{suffix}"), s));
        }
    }
    print!("{}", table(&configs));
    for (c, s) in &configs {
        println!("{c}: load average {}", s.load.join(" -> "));
    }
    let path = o
        .out
        .unwrap_or_else(|| ws.join("target/bench/results.json"));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&path, json(&configs)).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("bench: wrote {}", path.display());
    Ok(())
}
