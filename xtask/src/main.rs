//! `cargo xtask <task>`:
//!
//! - `build-root`: build every guest crate under `appl/` for `wasm32-wasip1`,
//!   check each module, and install commands as `root/bin/<name>.wasm` and
//!   tests as `root/bin/test/<name>.wasm`.
//! - `run-emu [-- emu args]`: `build-root`, build `emu`, and run it on
//!   `root/` with stdin, stdout and stderr passed through.
//! - `probe`: `build-root`, build `purg-probe` for `aarch64-unknown-none`,
//!   boot it in `qemu-system-aarch64` (or `$QEMU`) and check its serial
//!   output and semihosting exit code.
//! - `check-wasm <file.wasm>...`: check modules without building.

mod wasm;

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const GUEST_TARGET: &str = "wasm32-wasip1";
const GUEST_PROFILE: &str = "wasm";

type Result<T> = std::result::Result<T, String>;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

/// A guest crate: its Cargo package and the binary it produces.
struct Guest {
    package: String,
    bin: String,
    /// Install directory under `root/`.
    dir: &'static str,
}

/// First `name = "..."` value after the `[section]` header in a manifest.
fn manifest_name(manifest: &str, section: &str) -> Option<String> {
    let mut in_section = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == section;
        } else if in_section && let Some(v) = line.strip_prefix("name") {
            let v = v.trim_start().strip_prefix('=')?.trim();
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}

fn guests(ws: &Path) -> Result<Vec<Guest>> {
    let mut out = Vec::new();
    for (sub, dir) in [("appl/cmd", "bin"), ("appl/test", "bin/test")] {
        let mut entries: Vec<_> = fs::read_dir(ws.join(sub))
            .map_err(|e| format!("{sub}: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            let Ok(manifest) = fs::read_to_string(path.join("Cargo.toml")) else {
                continue;
            };
            let package = manifest_name(&manifest, "[package]")
                .ok_or_else(|| format!("{}: no package name", path.display()))?;
            let bin = manifest_name(&manifest, "[[bin]]").unwrap_or_else(|| package.clone());
            out.push(Guest { package, bin, dir });
        }
    }
    Ok(out)
}

fn cargo() -> Command {
    Command::new(env::var("CARGO").unwrap_or_else(|_| String::from("cargo")))
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} failed: {status}"))
    }
}

/// Rustflags that make guest modules byte-identical across build hosts by
/// removing host-specific paths from panic messages and debug info.
fn reproducible_rustflags(ws: &Path) -> String {
    let mut flags = vec![format!("--remap-path-prefix={}=/purgatorio", ws.display())];
    let cargo_home = env::var("CARGO_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| env::var("HOME").ok().map(|h| Path::new(&h).join(".cargo")));
    if let Some(home) = cargo_home {
        flags.push(format!("--remap-path-prefix={}=/cargo", home.display()));
    }
    if let Ok(rustup) = env::var("RUSTUP_HOME") {
        flags.push(format!("--remap-path-prefix={rustup}=/rustup"));
    }
    flags.join("\x1f")
}

fn build_root() -> Result<()> {
    let ws = workspace();
    let guests = guests(&ws)?;
    let mut cmd = cargo();
    cmd.current_dir(&ws)
        .args([
            "build",
            "--target",
            GUEST_TARGET,
            "--profile",
            GUEST_PROFILE,
        ])
        .env("CARGO_ENCODED_RUSTFLAGS", reproducible_rustflags(&ws))
        .env(WRAPPER_ENV, "1")
        .env(
            "RUSTC_WORKSPACE_WRAPPER",
            env::current_exe().map_err(|e| format!("xtask path: {e}"))?,
        );
    for g in &guests {
        cmd.args(["-p", &g.package]);
    }
    run(&mut cmd)?;
    let out_dir = ws.join("target").join(GUEST_TARGET).join(GUEST_PROFILE);
    for g in &guests {
        let src = out_dir.join(format!("{}.wasm", g.bin));
        let bytes = fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        wasm::check(&bytes, true).map_err(|e| format!("{}: {e}", src.display()))?;
        let dest_dir = ws.join("root").join(g.dir);
        fs::create_dir_all(&dest_dir).map_err(|e| format!("{}: {e}", dest_dir.display()))?;
        let dest = dest_dir.join(format!("{}.wasm", g.bin));
        fs::write(&dest, &bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
        eprintln!(
            "installed /{}/{}.wasm ({} bytes)",
            g.dir,
            g.bin,
            bytes.len()
        );
    }
    Ok(())
}

fn run_emu(args: &[String]) -> Result<ExitCode> {
    build_root()?;
    let ws = workspace();
    run(cargo()
        .current_dir(&ws)
        .args(["build", "--quiet", "-p", "purg-emu"]))?;
    let emu = ws.join("target/debug/emu");
    let status = Command::new(&emu)
        .arg("-r")
        .arg(ws.join("root"))
        .args(args)
        .status()
        .map_err(|e| format!("{}: {e}", emu.display()))?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

const PROBE_TARGET: &str = "aarch64-unknown-none";
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds `purg-probe` over a fresh `root/`, boots it in QEMU and compares
/// its serial output with `crates/purg-probe/expected.out`.
fn probe() -> Result<()> {
    build_root()?;
    let ws = workspace();
    run(cargo().current_dir(&ws).args([
        "build",
        "-p",
        "purg-probe",
        "--target",
        PROBE_TARGET,
        "--profile",
        "probe",
    ]))?;
    let image = ws
        .join("target")
        .join(PROBE_TARGET)
        .join("probe/purg-probe");
    let qemu = env::var("QEMU").unwrap_or_else(|_| String::from("qemu-system-aarch64"));
    let mut child = Command::new(&qemu)
        .args([
            "-M",
            "virt",
            "-cpu",
            "cortex-a72",
            "-m",
            "512M",
            "-nographic",
            "-semihosting",
            "-kernel",
        ])
        .arg(&image)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{qemu}: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| format!("{qemu}: {e}"))? {
            break s;
        }
        if start.elapsed() > PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let out = reader.join().unwrap_or_default();
            eprint!("{}", String::from_utf8_lossy(&out));
            return Err(format!("probe did not exit within {PROBE_TIMEOUT:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let elapsed = start.elapsed();
    let out = reader.join().map_err(|_| "reading QEMU output failed")?;
    let got = String::from_utf8_lossy(&out).replace('\r', "");
    print!("{got}");
    let golden = ws.join("crates/purg-probe/expected.out");
    let want = fs::read_to_string(&golden).map_err(|e| format!("{}: {e}", golden.display()))?;
    if !status.success() {
        return Err(format!("probe exited with {status}"));
    }
    if got != want {
        return Err(format!(
            "probe output differs from {}; expected:\n{want}",
            golden.display()
        ));
    }
    eprintln!("probe: passed in {:.2}s", elapsed.as_secs_f64());
    Ok(())
}

fn check_wasm(files: &[String]) -> Result<()> {
    for f in files {
        let bytes = fs::read(f).map_err(|e| format!("{f}: {e}"))?;
        wasm::check(&bytes, false).map_err(|e| format!("{f}: {e}"))?;
        println!("{f}: ok");
    }
    Ok(())
}

/// Set when xtask runs as cargo's `RUSTC_WORKSPACE_WRAPPER` for guest builds.
const WRAPPER_ENV: &str = "PURG_XTASK_RUSTC_WRAPPER";

/// Runs rustc with every `-C metadata=<hash>` replaced by one derived from
/// the crate name alone. On stable, Cargo's hash covers `rustc -vV`,
/// including the host triple; rustc orders generic instantiations by symbol
/// name, which contains that hash, so the build host would otherwise change
/// function order in guest modules.
fn rustc_wrapper(args: &[String]) -> ExitCode {
    let Some((rustc, rest)) = args.split_first() else {
        eprintln!("xtask: rustc wrapper called without rustc");
        return ExitCode::FAILURE;
    };
    let crate_name = rest
        .iter()
        .position(|a| a == "--crate-name")
        .and_then(|i| rest.get(i + 1))
        .cloned();
    let mut out = Vec::with_capacity(rest.len());
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), &crate_name) {
            ("-C", Some(name)) => {
                let v = it.next().map(String::as_str).unwrap_or_default();
                out.push(a.clone());
                if v.starts_with("metadata=") {
                    out.push(format!("metadata=purgatorio-{name}"));
                } else {
                    out.push(v.to_string());
                }
            }
            (s, Some(name)) if s.starts_with("-Cmetadata=") => {
                out.push(format!("-Cmetadata=purgatorio-{name}"));
            }
            _ => out.push(a.clone()),
        }
    }
    match Command::new(rustc).args(&out).status() {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("xtask: {rustc}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if env::var_os(WRAPPER_ENV).is_some() {
        return rustc_wrapper(&args);
    }
    let rest: Vec<String> = args
        .iter()
        .skip(1)
        .filter(|a| *a != "--")
        .cloned()
        .collect();
    let res = match args.first().map(String::as_str) {
        Some("build-root") => build_root().map(|()| ExitCode::SUCCESS),
        Some("run-emu") => {
            // Everything after `--` goes to emu untouched, including `--`-like args.
            let emu_args = match args.iter().position(|a| a == "--") {
                Some(i) => args[i + 1..].to_vec(),
                None => rest,
            };
            run_emu(&emu_args)
        }
        Some("check-wasm") => check_wasm(&rest).map(|()| ExitCode::SUCCESS),
        Some("probe") => probe().map(|()| ExitCode::SUCCESS),
        _ => Err(String::from(
            "usage: cargo xtask build-root | run-emu [-- emu args] | probe | check-wasm file.wasm...",
        )),
    };
    match res {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}
