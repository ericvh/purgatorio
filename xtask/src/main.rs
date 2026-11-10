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
//! - `kernel [--kvm]`: build `purg-kernel` for `aarch64-unknown-none` and
//!   boot it under QEMU TCG with GICv2, GICv3, and GICv3 at EL2 (or, with
//!   `--kvm`, once under KVM), checking each run's exit status and serial
//!   output against `crates/purg-kernel/expected.out`.
//! - `initrd`: `build-root`, then pack `root/bin` and `root/lib` into a
//!   deterministic ustar archive at `target/initrd/root.tar`, the kernel's
//!   root file system.
//! - `check-wasm <file.wasm>...`: check modules without building.
//! - `clippy-guests`: run clippy on every guest crate and the SDK for
//!   `wasm32-wasip1`, denying warnings.

mod tar;
mod wasm;

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
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
    dir: String,
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
    for (sub, dir) in [
        ("appl/cmd", "bin"),
        ("appl/test", "bin/test"),
        ("appl/lib", "bin/lib"),
    ] {
        let mut entries: Vec<_> = fs::read_dir(ws.join(sub))
            .map_err(|e| format!("{sub}: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.join("Cargo.toml").is_file() {
                // appl/lib also holds plain libraries linked into commands
                // (regexp); only reactor modules are installed.
                if sub == "appl/lib" && !is_reactor(&path)? {
                    continue;
                }
                out.push(guest(&path, String::from(dir))?);
                continue;
            }
            // A directory of crates, such as appl/cmd/ndb, installs into a
            // subdirectory: appl/cmd/ndb/query is /bin/ndb/query.wasm.
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(sub) = fs::read_dir(&path) else {
                continue;
            };
            let mut inner: Vec<_> = sub.filter_map(|e| e.ok().map(|e| e.path())).collect();
            inner.sort();
            for p in inner {
                if p.join("Cargo.toml").is_file() {
                    out.push(guest(&p, format!("{dir}/{name}"))?);
                }
            }
        }
    }
    Ok(out)
}

/// Whether the crate at `path` builds a `cdylib`, i.e. a loadable reactor.
fn is_reactor(path: &Path) -> Result<bool> {
    let manifest = fs::read_to_string(path.join("Cargo.toml"))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(manifest
        .lines()
        .any(|l| l.trim_start().starts_with("crate-type") && l.contains("cdylib")))
}

fn guest(path: &Path, dir: String) -> Result<Guest> {
    let manifest = fs::read_to_string(path.join("Cargo.toml"))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let package = manifest_name(&manifest, "[package]")
        .ok_or_else(|| format!("{}: no package name", path.display()))?;
    let bin = manifest_name(&manifest, "[[bin]]")
        .or_else(|| manifest_name(&manifest, "[lib]"))
        .unwrap_or_else(|| package.clone());
    Ok(Guest { package, bin, dir })
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
    let tmp = ws.join("root/tmp");
    fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let out_dir = ws.join("target").join(GUEST_TARGET).join(GUEST_PROFILE);
    for g in &guests {
        let src = out_dir.join(format!("{}.wasm", g.bin));
        let bytes = fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        wasm::check(&bytes, true).map_err(|e| format!("{}: {e}", src.display()))?;
        let dest_dir = ws.join("root").join(&g.dir);
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

fn clippy_guests() -> Result<()> {
    let ws = workspace();
    let mut cmd = cargo();
    cmd.current_dir(&ws)
        .args(["clippy", "--target", GUEST_TARGET, "-p", "purgatorio"]);
    for g in guests(&ws)? {
        cmd.args(["-p", &g.package]);
    }
    run(cmd.args(["--", "-D", "warnings"]))
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
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// Host port forwarded to the probe's `tcp!*!6666` export.
const PROBE_NET_PORT: u16 = 17666;

/// A finished QEMU run.
struct QemuRun {
    status: std::process::ExitStatus,
    /// The serial output, with `\r` removed.
    serial: String,
    elapsed: Duration,
}

/// Boots `image` with `qemu-system-aarch64` (or `$QEMU`) and `args` (which
/// must end with `-kernel`), collecting the serial console from stdout.
/// Every 20 ms `watch` sees the output so far; an error from it, or the
/// run outliving `timeout`, kills QEMU, prints the output and fails.
fn run_qemu(
    args: &[&str],
    image: &Path,
    timeout: Duration,
    mut watch: impl FnMut(&str) -> Result<()>,
) -> Result<QemuRun> {
    let qemu = env::var("QEMU").unwrap_or_else(|_| String::from("qemu-system-aarch64"));
    let mut child = Command::new(&qemu)
        .args(args)
        .arg(image)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{qemu}: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let serial = Arc::new(Mutex::new(Vec::<u8>::new()));
    let serial_for_reader = Arc::clone(&serial);
    let reader = std::thread::spawn(move || {
        let mut tmp = [0u8; 512];
        loop {
            match stdout.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => serial_for_reader
                    .lock()
                    .expect("serial")
                    .extend_from_slice(&tmp[..n]),
            }
        }
    });
    let text = || String::from_utf8_lossy(&serial.lock().expect("serial")).replace('\r', "");
    let start = Instant::now();
    let status = loop {
        let err = match watch(&text()) {
            Err(e) => Some(e),
            Ok(()) if start.elapsed() > timeout => {
                Some(format!("QEMU did not exit within {timeout:?}"))
            }
            Ok(()) => None,
        };
        if let Some(e) = err {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            eprint!("{}", text());
            return Err(e);
        }
        if let Some(s) = child.try_wait().map_err(|e| format!("{qemu}: {e}"))? {
            break s;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = start.elapsed();
    let _ = reader.join();
    Ok(QemuRun {
        status,
        serial: text(),
        elapsed,
    })
}

/// Directories under `root/` packed into the initrd.
const INITRD_DIRS: &[&str] = &["bin", "lib"];

/// Runs `build-root` and writes `target/initrd/root.tar` from `root/bin` and
/// `root/lib`, returning its path. The archive is byte-identical for the
/// same `root/`.
fn initrd() -> Result<PathBuf> {
    build_root()?;
    let ws = workspace();
    let tar = tar::archive(&ws.join("root"), INITRD_DIRS)?;
    let dir = ws.join("target/initrd");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("root.tar");
    fs::write(&path, &tar).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("wrote {} ({} bytes)", path.display(), tar.len());
    Ok(path)
}

/// Builds `purg-probe` over a fresh `root/`, boots it in QEMU and compares
/// its serial output with `crates/purg-probe/expected.out`.
fn probe() -> Result<()> {
    build_root()?;
    let ws = workspace();
    run(cargo().current_dir(&ws).args(["build", "-p", "purg-emu"]))?;
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
    let fwd = format!("user,id=net0,hostfwd=tcp:127.0.0.1:{PROBE_NET_PORT}-:6666");
    let args = [
        "-M",
        "virt",
        "-cpu",
        "cortex-a72",
        "-m",
        "512M",
        "-nographic",
        "-semihosting",
        "-netdev",
        &fwd,
        "-global",
        "virtio-mmio.force-legacy=off",
        "-device",
        "virtio-net-device,netdev=net0,mac=52:54:00:12:34:56",
        "-kernel",
    ];
    let mut mounted = false;
    let r = run_qemu(&args, &image, PROBE_TIMEOUT, |text| {
        if !mounted && text.contains("probe: net ready\n") {
            mounted = true;
            mount_probe(&ws)?;
        }
        Ok(())
    })?;
    print!("{}", r.serial);
    let golden = ws.join("crates/purg-probe/expected.out");
    let want = fs::read_to_string(&golden).map_err(|e| format!("{}: {e}", golden.display()))?;
    let want = expand_golden(&want, "probe", &ws)?;
    if !r.status.success() {
        return Err(format!("probe exited with {}", r.status));
    }
    if r.serial != want {
        return Err(format!(
            "probe output differs from {}; expected:\n{want}",
            golden.display()
        ));
    }
    if !mounted {
        return Err(String::from("probe finished without announcing tcp!*!6666"));
    }
    eprintln!("probe: passed in {:.2}s", r.elapsed.as_secs_f64());
    Ok(())
}

/// The sh corpus: the `.sh` files in `root/lib/test/sh`, sorted, as the
/// probe and the kernel run them.
const SH_CORPUS: &str = "lib/test/sh";

/// Expands a golden file's `@sh-corpus` line into the lines the probe or
/// kernel prints for each corpus script (`<prefix>: sh /lib/test/sh/x.sh`
/// then `<prefix>: ok`). Listing the scripts here rather than in each
/// golden file means a new script changes no golden file, so branches that
/// add scripts do not conflict.
fn expand_golden(text: &str, prefix: &str, ws: &Path) -> Result<String> {
    let dir = ws.join("root").join(SH_CORPUS);
    let mut scripts: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".sh"))
        .collect();
    scripts.sort();
    let mut out = String::new();
    for line in text.lines() {
        if line == "@sh-corpus" {
            for s in &scripts {
                out.push_str(&format!("{prefix}: sh /{SH_CORPUS}/{s}\n{prefix}: ok\n"));
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Running the conformance guests and the sh corpus under wasmi in TCG
/// takes about as long as the probe.
const KERNEL_TIMEOUT: Duration = Duration::from_secs(120);

/// One QEMU configuration `xtask kernel` boots: a name, the machine
/// arguments before `-kernel`, and the exception level the kernel must
/// report booting at.
struct KernelRun {
    name: &'static str,
    args: &'static [&'static str],
    el: u32,
    /// Boot with `-initrd root.tar` instead of the compiled-in root.
    initrd: bool,
}

/// The TCG runs: GICv2 and GICv3 at EL1, and GICv3 with EL2
/// (`virtualization=on`), which exercises the EL2-to-EL1 path, on a
/// Cortex-A72 and on `-cpu max` (every feature QEMU has, to catch
/// assumptions about ID registers). They exit through semihosting.
const KERNEL_TCG_RUNS: &[KernelRun] = &[
    KernelRun {
        name: "tcg cortex-a72 gicv2 initrd",
        args: &["-M", "virt,gic-version=2", "-cpu", "cortex-a72"],
        el: 1,
        initrd: true,
    },
    KernelRun {
        name: "tcg cortex-a72 gicv3",
        args: &["-M", "virt,gic-version=3", "-cpu", "cortex-a72"],
        el: 1,
        initrd: false,
    },
    KernelRun {
        name: "tcg cortex-a72 gicv3 el2",
        args: &[
            "-M",
            "virt,gic-version=3,virtualization=on",
            "-cpu",
            "cortex-a72",
        ],
        el: 2,
        initrd: false,
    },
    KernelRun {
        name: "tcg max gicv3 el2",
        args: &["-M", "virt,gic-version=3,virtualization=on", "-cpu", "max"],
        el: 2,
        initrd: false,
    },
];

/// The KVM run (aarch64 Linux hosts with /dev/kvm). KVM has no
/// semihosting, GICv2 emulation or nested EL2 here, so the kernel exits
/// through PSCI `SYSTEM_OFF` (`exit=psci`), QEMU's status is 0 either way,
/// and pass/fail comes from the serial output.
const KERNEL_KVM_RUN: KernelRun = KernelRun {
    name: "kvm host gicv3 initrd",
    args: &["-accel", "kvm", "-cpu", "host", "-M", "virt,gic-version=3"],
    el: 1,
    initrd: true,
};

/// Turns the kernel's ELF into a raw arm64 `Image`: each loadable segment
/// copied to its physical address, relative to the lowest one. The image
/// starts with the arm64 Image header (see the kernel's boot.rs), so QEMU
/// loads it Linux-style, which is what makes it load `-initrd`.
fn elf_to_image(elf: &[u8]) -> Result<Vec<u8>> {
    let u16_at = |o: usize| elf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u64_at = |o: usize| {
        elf.get(o..o + 8)
            .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
    };
    if elf.get(..6) != Some(b"\x7fELF\x02\x01".as_slice()) {
        return Err(String::from("kernel: not a 64-bit little-endian ELF"));
    }
    let bad = || String::from("kernel: truncated ELF");
    let phoff = u64_at(0x20).ok_or_else(bad)? as usize;
    let phentsize = u16_at(0x36).ok_or_else(bad)? as usize;
    let phnum = u16_at(0x38).ok_or_else(bad)? as usize;
    let mut segs = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let kind = elf.get(ph..ph + 4).ok_or_else(bad)?;
        if kind != 1u32.to_le_bytes() {
            continue;
        }
        let offset = u64_at(ph + 0x08).ok_or_else(bad)? as usize;
        let paddr = u64_at(ph + 0x18).ok_or_else(bad)?;
        let filesz = u64_at(ph + 0x20).ok_or_else(bad)? as usize;
        if filesz > 0 {
            segs.push((paddr, elf.get(offset..offset + filesz).ok_or_else(bad)?));
        }
    }
    let base = segs
        .iter()
        .map(|s| s.0)
        .min()
        .ok_or("kernel: no loadable segments")?;
    let mut out = Vec::new();
    for (paddr, data) in segs {
        let at = (paddr - base) as usize;
        if out.len() < at + data.len() {
            out.resize(at + data.len(), 0);
        }
        out[at..at + data.len()].copy_from_slice(data);
    }
    if out.get(56..60) != Some(b"ARMd".as_slice()) {
        return Err(String::from("kernel: the image has no arm64 Image header"));
    }
    Ok(out)
}

/// Builds `purg-kernel` and boots it under each QEMU configuration (TCG,
/// or KVM with `--kvm`), checking the exit status, the boot EL line and
/// the rest of the serial output against `crates/purg-kernel/expected.out`.
fn kernel(kvm: bool) -> Result<()> {
    let ws = workspace();
    let initrd = initrd()?;
    let initrd = initrd
        .to_str()
        .ok_or("initrd path is not UTF-8")?
        .to_owned();
    run(cargo().current_dir(&ws).args([
        "build",
        "-p",
        "purg-kernel",
        "--target",
        PROBE_TARGET,
        "--profile",
        "kernel",
    ]))?;
    let image = ws
        .join("target")
        .join(PROBE_TARGET)
        .join("kernel/purg-kernel");
    let elf = fs::read(&image).map_err(|e| format!("{}: {e}", image.display()))?;
    let image = image.with_extension("img");
    fs::write(&image, elf_to_image(&elf)?).map_err(|e| format!("{}: {e}", image.display()))?;
    let golden = ws.join("crates/purg-kernel/expected.out");
    let want = fs::read_to_string(&golden).map_err(|e| format!("{}: {e}", golden.display()))?;
    let runs: Vec<&KernelRun> = if kvm {
        if env::consts::ARCH != "aarch64" || !Path::new("/dev/kvm").exists() {
            eprintln!("kernel: skipping the KVM run: needs an aarch64 host with /dev/kvm");
            return Ok(());
        }
        vec![&KERNEL_KVM_RUN]
    } else {
        KERNEL_TCG_RUNS.iter().collect()
    };
    let mut failed = Vec::new();
    for k in runs {
        let mut args: Vec<&str> = k.args.to_vec();
        args.extend(["-m", "512M", "-nographic"]);
        if k.initrd {
            args.extend(["-initrd", &initrd]);
        }
        // `test` runs the conformance guests and the sh corpus instead of
        // an interactive shell.
        if kvm {
            args.extend(["-append", "exit=psci test"]);
        } else {
            args.extend(["-semihosting", "-append", "test"]);
        }
        args.push("-kernel");
        eprintln!("kernel: {}: qemu-system-aarch64 {}", k.name, args.join(" "));
        let r = match run_qemu(&args, &image, KERNEL_TIMEOUT, |_| Ok(())) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("kernel: {}: {e}", k.name);
                failed.push(k.name);
                continue;
            }
        };
        print!("{}", r.serial);
        let el_line = format!("kernel: booted at EL{}", k.el);
        let root_line = if k.initrd {
            "kernel: root from the initrd"
        } else {
            "kernel: root compiled in"
        };
        let rest: String = r
            .serial
            .lines()
            .filter(|l| !l.starts_with("kernel: booted at EL") && !l.starts_with("kernel: root "))
            .flat_map(|l| [l, "\n"])
            .collect();
        let problem = if !r.status.success() {
            Some(format!("QEMU exited with {}", r.status))
        } else if !r.serial.lines().any(|l| l == el_line) {
            Some(format!("no `{el_line}` line"))
        } else if !r.serial.lines().any(|l| l == root_line) {
            Some(format!("no `{root_line}` line"))
        } else if rest != want {
            Some(format!(
                "output differs from {}; expected (less the EL line):\n{want}",
                golden.display()
            ))
        } else {
            None
        };
        match problem {
            Some(p) => {
                eprintln!("kernel: {}: FAILED: {p}", k.name);
                failed.push(k.name);
            }
            None => eprintln!(
                "kernel: {}: passed in {:.2}s",
                k.name,
                r.elapsed.as_secs_f64()
            ),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("kernel runs failed: {}", failed.join(", ")))
    }
}

/// Mounts the probe's export from this host and reads a file through it.
fn mount_probe(ws: &Path) -> Result<()> {
    let emu = ws.join("target/debug/emu");
    let script = format!(
        "mkdir -p /tmp/probe-mnt; mount tcp!127.0.0.1!{PROBE_NET_PORT} /tmp/probe-mnt; cat /tmp/probe-mnt/lib/probe/hello.txt"
    );
    let mut child = Command::new(&emu)
        .arg("-r")
        .arg(ws.join("root"))
        .arg("--vm=wasmi")
        .args(["sh", "-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", emu.display()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("emu: {e}"))? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(String::from("probe export mount timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    let out_status = status;
    if stdout.trim() != "hello from the probe root" {
        return Err(format!(
            "probe export mount failed ({out_status})\n{stdout}{stderr}"
        ));
    }
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
        Some("initrd") => initrd().map(|_| ExitCode::SUCCESS),
        Some("check-wasm") => check_wasm(&rest).map(|()| ExitCode::SUCCESS),
        Some("probe") => probe().map(|()| ExitCode::SUCCESS),
        Some("kernel") => match rest.as_slice() {
            [] => kernel(false).map(|()| ExitCode::SUCCESS),
            [f] if f == "--kvm" => kernel(true).map(|()| ExitCode::SUCCESS),
            _ => Err(String::from("usage: cargo xtask kernel [--kvm]")),
        },
        Some("clippy-guests") => clippy_guests().map(|()| ExitCode::SUCCESS),
        _ => Err(String::from(
            "usage: cargo xtask build-root | run-emu [-- emu args] | probe | kernel [--kvm] | initrd | clippy-guests | check-wasm file.wasm...",
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
