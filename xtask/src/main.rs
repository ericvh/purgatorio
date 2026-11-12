//! `cargo xtask <task>`:
//!
//! - `build-root`: build every guest crate under `appl/` for `wasm32-wasip1`,
//!   check each module, and install commands as `root/bin/<name>.wasm` and
//!   tests as `root/bin/test/<name>.wasm`, each with its function names
//!   beside it in `<name>.names` (`docs/debug.md`).
//! - `lethed [-- lethed args]`: build `lethed` and run it (`docs/lethe.md`).
//! - `lethe-perf [--reps N] [--parts ...]`: the Lethe and Styx study
//!   (`docs/lethe-perf.md`); not a test.
//! - `run-emu [-- emu args]`: `build-root`, build `emu`, and run it on
//!   `root/` with stdin, stdout and stderr passed through.
//! - `probe [--kvm] [--names]`: `build-root`, build `purg-probe` for
//!   `aarch64-unknown-none`, boot it in `qemu-system-aarch64` (or `$QEMU`)
//!   under TCG (or, with `--kvm`, under KVM) and check its serial output
//!   and exit status.
//! - `kernel [--kvm] [--names] [--mem SIZE] [--smp N]`: build `purg-kernel` for `aarch64-unknown-none` and
//!   boot it under QEMU TCG with GICv2, GICv3, and GICv3 at EL2 (or, with
//!   `--kvm`, once under KVM), checking each run's exit status and serial
//!   output against `crates/purg-kernel/expected.out`. Each run types
//!   an edited line and raw bytes on the serial console for the console
//!   self-test, checks the echo, and
//!   has a virtio-net card on QEMU user networking: the kernel mounts a
//!   hosted emu's `styxlisten` export and pings a UDP echo server on this
//!   host, attaches this host's `lethed` (`docs/lethe.md`), and this host
//!   mounts the kernel's `tcp!*!6666` export and runs
//!   an MCP session on the kernel with `rcmd tcp!...!6668 mcpd`. Each run
//!   also shares a fresh directory under `target/kernel-9p/` over
//!   virtio-9p, which the kernel mounts on `/n/host` and works on, and
//!   whose final contents are checked here. The whole sh corpus runs on
//!   GICv2 and on `-cpu max`, and a smoke subset on the other two
//!   (`KERNEL_CORPUS=full` runs it on all). The runs boot side by side,
//!   one per two CPUs (`KERNEL_JOBS=N` sets how many), and each prints
//!   where its time went; `target/qemu-times/` has each console with the
//!   time each line arrived.
//!   `--mem` sets QEMU's `-m` (512M by default), `--smp` its `-smp` (1 by
//!   default; the kernel must report that many CPUs online). With N CPUs
//!   each run keeps N host threads busy, so N times fewer boot at once,
//!   and the timeouts are longer. `--stress SECS`
//!   boots once instead (under KVM with `--kvm`, else the first TCG machine), in
//!   the kernel's stress mode for SECS seconds, printing its console as it
//!   runs. `KERNEL_MEMSTAT=1` in
//!   the environment adds `memstat` to the bootargs: the kernel then
//!   prints `kernel: mem ...` lines about its memory use, which the
//!   comparison skips.
//! - `initrd [--names]`: `build-root`, then pack `root/bin` and `root/lib`
//!   into a deterministic ustar archive at `target/initrd/root.tar`, the
//!   kernel's root file system.
//! - `diskimg [--size SIZE] [--bsize N] OUT [DIR]`: make `OUT` a purgfs
//!   disk image (`docs/fs.md`) of `SIZE` bytes (64 MiB by default; `k`,
//!   `m` and `g` suffixes) holding a copy of `DIR`, with `purg-fs`, the
//!   format the kernel mounts. `diskimg --check IMAGE` checks one.
//! - `size [--guests] [--small]`: build `purg-kernel` and print its section
//!   sizes, its code by crate, and the sizes of the guests in `root/bin`
//!   (with `--guests`, one line per module). `--small` builds the
//!   smallest kernel: the `kernel-small` profile, without the optional
//!   features (TLS, virtio-9p, networking).
//!   `size --elf FILE` reports on a kernel built earlier.
//!   See `docs/footprint.md`.
//! - `bench [--emu] [--kernel] [--kvm] [--tcg] [--vm=wasmtime,wasmi]
//!   [--rounds N] [--boots N] [--features F,...] [--profile P] [--label L]
//!   [--strict-align] [--rustflags FLAGS] [--config K=V] [--aot native|pulley]
//!   [--out FILE]`: run the benchmark suite (`root/lib/bench/bench.sh`,
//!   `docs/perf.md`) on a release emu with each engine and on the kernel
//!   under KVM (or TCG), print each benchmark's median and spread, and
//!   write the samples to `target/bench/results.json`.
//! - `small`: build the experimental `no_std` and multi-call guests in
//!   `appl/small` (its own workspace) and compare their sizes with the
//!   `std` commands in `root/bin`.
//!
//! The probe's and the kernel's images leave out the guests' `.names`
//! files, except the test guests'; `--names` puts them all in, so that
//! `stack` and `prof` name functions on bare metal (`docs/debug.md`).
//! Both images are built without strict alignment, except for the crates
//! whose code runs with the MMU off, and each build checks that boot path
//! (`align.rs`).
//! - `check-wasm <file.wasm>...`: check modules without building.
//! - `clippy-guests`: run clippy on every guest crate and the SDK for
//!   `wasm32-wasip1`, and on the experimental guests in `appl/small`,
//!   denying warnings.

mod align;
mod bench;
mod disk;
mod kprof;
mod letheperf;
mod phases;
mod size;
mod tar;
mod wasm;

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const GUEST_TARGET: &str = "wasm32-wasip1";
const GUEST_PROFILE: &str = "wasm";

/// The guests' shadow stack, in bytes: the part of a module's linear
/// memory its compiled code uses as a stack. The linker's default is
/// 1 MiB, which every running prog then pays for. The sh corpus passes
/// with 128 KiB, and all of it but `gzip` and the deepest nesting `sh`
/// accepts with 16 KiB, so this leaves at least twice what anything is
/// known to need. The stack is below the data, so running out of it is a
/// trap, not corruption. A crate that needs more can say so in a build
/// script (`cargo:rustc-link-arg-bins=-zstack-size=N`, for wasm targets
/// only), which wins over this. `PURG_GUEST_STACK` in the environment
/// overrides it, for measuring (docs/footprint.md).
const GUEST_STACK: u32 = 256 << 10;

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

impl Guest {
    /// The installed file's name. A binary in a directory of commands may
    /// carry the directory as a prefix, so that it does not collide with a
    /// top-level command's binary: `ndb-cs` in `bin/ndb` installs as
    /// `bin/ndb/cs.wasm`, beside `bin/cs.wasm`.
    fn installed(&self) -> &str {
        let sub = self.dir.rsplit('/').next().unwrap_or("");
        self.bin
            .strip_prefix(sub)
            .and_then(|b| b.strip_prefix('-'))
            .filter(|b| !b.is_empty() && self.dir.contains('/'))
            .unwrap_or(&self.bin)
    }
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

/// The rustflags for guest builds: the reproducible ones, and the guests'
/// shadow stack size (see [`GUEST_STACK`]).
fn guest_rustflags(ws: &Path) -> Result<String> {
    let stack = match env::var("PURG_GUEST_STACK") {
        Ok(v) => v
            .parse::<u32>()
            .map_err(|_| format!("PURG_GUEST_STACK={v}: not a number of bytes"))?,
        Err(_) => GUEST_STACK,
    };
    Ok(format!(
        "{}\x1f-Clink-arg=-zstack-size={stack}",
        reproducible_rustflags(ws)
    ))
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
        .env("CARGO_ENCODED_RUSTFLAGS", guest_rustflags(&ws)?)
        .env(WRAPPER_ENV, "1")
        // `RUSTC_WRAPPER`, not `RUSTC_WORKSPACE_WRAPPER`: the crates from
        // the registry need the same treatment as the workspace's.
        .env(
            "RUSTC_WRAPPER",
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
        let linked = fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        // The linker keeps the function names (`strip = "debuginfo"`);
        // they are installed beside the module, which is stripped here.
        let (bytes, names) = wasm::split(&linked).map_err(|e| format!("{}: {e}", src.display()))?;
        wasm::check(&bytes, true).map_err(|e| format!("{}: {e}", src.display()))?;
        let dest_dir = ws.join("root").join(&g.dir);
        fs::create_dir_all(&dest_dir).map_err(|e| format!("{}: {e}", dest_dir.display()))?;
        let dest = dest_dir.join(format!("{}.wasm", g.installed()));
        fs::write(&dest, &bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
        let dest = dest.with_extension("names");
        fs::write(&dest, &names).map_err(|e| format!("{}: {e}", dest.display()))?;
        eprintln!(
            "installed /{}/{}.wasm ({} bytes)",
            g.dir,
            g.installed(),
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
    run(cmd.args(["--", "-D", "warnings"]))?;
    // The experimental guests are a workspace of their own.
    for (triple, packages) in SMALL_BUILDS {
        let mut cmd = cargo();
        cmd.current_dir(ws.join("appl/small"))
            .args(["clippy", "--target", triple])
            .env("CARGO_TARGET_DIR", ws.join("target/small"));
        for p in *packages {
            cmd.args(["-p", p]);
        }
        run(cmd.args(["--", "-D", "warnings"]))?;
    }
    Ok(())
}

/// The packages of `appl/small`, by the target each is built for.
const SMALL_BUILDS: &[(&str, &[&str])] = &[
    (
        "wasm32-unknown-unknown",
        &[
            "small-rt",
            "small-echo",
            "small-cat",
            "small-sleep",
            "small-multi",
        ],
    ),
    (GUEST_TARGET, &["small-multi-std"]),
];

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

/// Builds `lethed` and runs it with `args` (`docs/lethe.md`), with stdin,
/// stdout and stderr passed through.
fn run_lethed(args: &[String]) -> Result<ExitCode> {
    let ws = workspace();
    run(cargo()
        .current_dir(&ws)
        .args(["build", "--quiet", "-p", "purg-emu", "--bin", "lethed"]))?;
    let lethed = ws.join("target/debug/lethed");
    let status = Command::new(&lethed)
        .args(args)
        .status()
        .map_err(|e| format!("{}: {e}", lethed.display()))?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

/// The host's `lethed` serving a kernel run's shared directory read-only
/// on a free port, trusting the test key and, with `-A`, answering
/// clear requests too; killed when dropped.
struct HostLethed {
    child: std::process::Child,
    port: u16,
}

impl HostLethed {
    /// `run` names the kernel run in what lethed prints on stderr.
    fn start(ws: &Path, tree: &Path, run: &str) -> Result<HostLethed> {
        let lethed = ws.join("target/debug/lethed");
        let mut child = Command::new(&lethed)
            .args(["-a", "127.0.0.1:0", "-A", "-k"])
            .arg(ws.join("root/lib/test/auth.key.pub"))
            .arg(tree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", lethed.display()))?;
        relay(
            child.stderr.take().expect("piped stderr"),
            format!("kernel: {run}: host lethed"),
        );
        let mut line = String::new();
        let out = child.stdout.take().expect("piped stdout");
        std::io::BufRead::read_line(&mut std::io::BufReader::new(out), &mut line)
            .map_err(|e| format!("lethed: {e}"))?;
        let mut host = HostLethed { child, port: 0 };
        host.port = line
            .trim()
            .strip_prefix("lethed: listening on ")
            .and_then(|a| a.rsplit(':').next())
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("lethed said {line:?}"))?;
        Ok(host)
    }

    /// What became of lethed, if it is not running any more.
    fn died(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        Some(format!(
            "the host's lethed exited during the run ({status})"
        ))
    }
}

/// Prints each line of a helper's `stream` on stderr after `label`, on a
/// thread of its own, until the helper closes it.
fn relay(stream: impl Read + Send + 'static, label: String) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            eprintln!("{label}: {line}");
        }
    });
}

impl Drop for HostLethed {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const PROBE_TARGET: &str = "aarch64-unknown-none";
/// A limit against hangs, not a performance target: a run takes about 80 s
/// under TCG on a busy machine, and it grows with the sh corpus.
const PROBE_TIMEOUT: Duration = Duration::from_secs(300);
/// The port the probe and the kernel export `/` on; QEMU forwards a free
/// host port to it.
const GUEST_EXPORT_PORT: u16 = 6666;
/// The port the kernel serves `auxi/rstyxd` on in test mode.
const GUEST_RSTYX_PORT: u16 = 6668;
/// The MAC address of the virtio-net card.
const GUEST_MAC: &str = "52:54:00:12:34:56";

/// A TCP port on 127.0.0.1 that is free now. Another process may take it
/// before it is used, but each run asking for its own means concurrent
/// runs (TCG and KVM, or two worktrees) do not collide.
fn free_port() -> Result<u16> {
    held_port().map(|(_, port)| port)
}

/// A free TCP port on 127.0.0.1 and the listener that holds it. Nothing
/// else that binds port 0 is given the port until the listener is
/// dropped, so hold it until just before the port's user starts: a port
/// let go early can go to the next helper that asks for one (#147: the
/// host's lethed was given the port QEMU was about to forward).
fn held_port() -> Result<(std::net::TcpListener, u16)> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("bind: {e}"))?;
    let port = l.local_addr().map_err(|e| format!("bind: {e}"))?.port();
    Ok((l, port))
}

/// What QEMU says when a `hostfwd` port is not free.
const HOSTFWD_REFUSED: &str = "Could not set up host forwarding rule";

/// How many times a run is started again on new ports after
/// [`HOSTFWD_REFUSED`].
const HOSTFWD_RETRIES: usize = 3;

/// QEMU arguments for a virtio-net card on user networking, with host port
/// `fwd` forwarded to the guest's export port, and `rstyx`, when given, to
/// its rstyx port.
fn net_args(fwd: u16, rstyx: Option<u16>) -> Vec<String> {
    let mut netdev = format!("user,id=net0,hostfwd=tcp:127.0.0.1:{fwd}-:{GUEST_EXPORT_PORT}");
    if let Some(r) = rstyx {
        netdev.push_str(&format!(",hostfwd=tcp:127.0.0.1:{r}-:{GUEST_RSTYX_PORT}"));
    }
    [
        "-netdev",
        &netdev,
        "-global",
        "virtio-mmio.force-legacy=off",
        "-device",
        &format!("virtio-net-device,netdev=net0,mac={GUEST_MAC}"),
    ]
    .map(String::from)
    .to_vec()
}

/// A finished QEMU run.
struct QemuRun {
    status: std::process::ExitStatus,
    /// The serial output, with `\r` removed.
    serial: String,
    /// What QEMU itself wrote on its standard error.
    stderr: String,
    elapsed: Duration,
    /// Each complete line of `serial` and when it arrived.
    lines: Vec<(Duration, String)>,
    /// Why the run was killed: an error from `watch`, or the timeout.
    killed: Option<String>,
}

impl QemuRun {
    /// QEMU's exit status as a failure, with what QEMU said, and whether
    /// the guest printed anything first: a status with no reason in the
    /// console is QEMU's own (#147).
    fn exit_problem(&self) -> Option<String> {
        if self.status.success() {
            return None;
        }
        let mut p = format!("QEMU exited with {}", self.status);
        if self.serial.is_empty() {
            p.push_str(" before the guest printed anything");
        }
        if !self.stderr.trim().is_empty() {
            p.push_str(&format!("; QEMU said: {}", self.stderr.trim()));
        }
        Some(p)
    }

    /// Whether QEMU could not start for a forwarded port that was taken.
    fn hostfwd_refused(&self) -> bool {
        !self.status.success() && self.serial.is_empty() && self.stderr.contains(HOSTFWD_REFUSED)
    }

    /// For a run that is not shown beside others: prints what QEMU wrote
    /// on its standard error, and, if the run was killed, its console,
    /// and fails.
    fn finished(self, who: &str) -> Result<QemuRun> {
        for l in self.stderr.lines() {
            eprintln!("{who}: qemu: {l}");
        }
        match &self.killed {
            Some(e) => {
                show_console(&self.serial);
                Err(e.clone())
            }
            None => Ok(self),
        }
    }
}

/// Prints a console on stdout, whole: ended with a newline if it stopped
/// in the middle of a line, and flushed, so that what is printed next on
/// stderr comes after it.
fn show_console(serial: &str) {
    print!("{serial}");
    if !serial.is_empty() && !serial.ends_with('\n') {
        println!();
    }
    let _ = std::io::stdout().flush();
}

/// Bytes to type on a QEMU run's serial console once a prompt appears.
struct SerialInput {
    prompt: &'static str,
    bytes: &'static [u8],
}

/// How long after the prompt the input is typed, so the guest's reader is
/// blocked (and its CPU idle) before the bytes arrive.
const INPUT_DELAY: Duration = Duration::from_millis(300);

/// The kernel console self-test's input (`purg-kernel`'s `selftest.rs`
/// holds what each must read as). Cooked: `junk` killed with ^U, then
/// `hexl` with the typo erased by DEL and ^H, `llo é`, a `ü` erased whole
/// by one DEL, CR, and ^D at the start of the next line, which reads as
/// end of input. Raw: bytes the cooked console would have acted on.
const KERNEL_INPUT: &[SerialInput] = &[
    SerialInput {
        prompt: "kernel: console: waiting for input\n",
        bytes: b"junk\x15hexl\x7f\x08llo \xc3\xa9\xc3\xbc\x7f\r\x04",
    },
    SerialInput {
        prompt: "kernel: console: waiting for raw input\n",
        bytes: b"a\x7f\x15\r\x04",
    },
];

/// What the kernel's cooked console echoes for [`KERNEL_INPUT`], after the
/// first prompt and with `\r` removed: each erased character is wiped with
/// backspace, space, backspace. The raw input is not echoed at all.
const KERNEL_ECHO: &str =
    "junk\x08 \x08\x08 \x08\x08 \x08\x08 \x08hexl\x08 \x08\x08 \x08llo éü\x08 \x08\n";

/// Boots `image` with `qemu-system-aarch64` (or `$QEMU`) and `args` (which
/// must end with `-kernel`), collecting the serial console from stdout.
/// Every 20 ms `watch` sees the output so far; an error from it, or the
/// run outliving `timeout`, kills QEMU, and the run comes back with
/// `killed` set. QEMU's standard error is collected too: the caller
/// prints both, so that runs side by side do not mix theirs.
/// With `input`, the serial console's input is a pipe, and each entry's
/// bytes are typed into it, in order, [`INPUT_DELAY`] after its prompt
/// line appears; otherwise the input is empty.
fn run_qemu(
    args: &[&str],
    image: &Path,
    timeout: Duration,
    input: &[SerialInput],
    mut watch: impl FnMut(&str) -> Result<()>,
) -> Result<QemuRun> {
    let qemu = env::var("QEMU").unwrap_or_else(|_| String::from("qemu-system-aarch64"));
    let mut child = Command::new(&qemu)
        .args(args)
        .arg(image)
        .stdin(if !input.is_empty() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{qemu}: {e}"))?;
    let mut qemu_stderr = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut said = Vec::new();
        let _ = qemu_stderr.read_to_end(&mut said);
        String::from_utf8_lossy(&said).into_owned()
    });
    // Held until QEMU exits: closing it would be end of file on the serial
    // input.
    let mut stdin = child.stdin.take();
    let mut prompted: Option<Instant> = None;
    let mut typed = 0;
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
    let mut lines = Vec::new();
    let mut stamped = 0;
    let mut killed = None;
    let status = loop {
        let now = text();
        // Complete lines are never rewritten: only a partial UTF-8
        // sequence at the very end can change.
        while let Some(n) = now[stamped..].find('\n') {
            lines.push((start.elapsed(), now[stamped..stamped + n].to_owned()));
            stamped += n + 1;
        }
        if let Some(inp) = input.get(typed) {
            if prompted.is_none() && now.contains(inp.prompt) {
                prompted = Some(Instant::now());
            }
            if prompted.is_some_and(|t| t.elapsed() >= INPUT_DELAY)
                && let Some(w) = stdin.as_mut()
            {
                if let Err(e) = w.write_all(inp.bytes).and_then(|()| w.flush()) {
                    // QEMU is gone or going: keep what it printed.
                    let _ = child.kill();
                    killed = Some(format!("{qemu}: writing the serial input: {e}"));
                    break child.wait().map_err(|e| format!("{qemu}: {e}"))?;
                }
                typed += 1;
                prompted = None;
            }
        }
        let err = match watch(&now) {
            Err(e) => Some(e),
            Ok(()) if start.elapsed() > timeout => {
                Some(format!("QEMU did not exit within {timeout:?}"))
            }
            Ok(()) => None,
        };
        if let Some(e) = err {
            let _ = child.kill();
            killed = Some(e);
            break child.wait().map_err(|e| format!("{qemu}: {e}"))?;
        }
        if let Some(s) = child.try_wait().map_err(|e| format!("{qemu}: {e}"))? {
            break s;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = start.elapsed();
    drop(stdin);
    let _ = reader.join();
    let serial = text();
    while let Some(n) = serial[stamped..].find('\n') {
        lines.push((elapsed, serial[stamped..stamped + n].to_owned()));
        stamped += n + 1;
    }
    Ok(QemuRun {
        status,
        serial,
        stderr: stderr_reader.join().unwrap_or_default(),
        elapsed,
        lines,
        killed,
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
    let tar = tar::archive(&ws.join("root"), INITRD_DIRS, tar::names_wanted())?;
    let dir = ws.join("target/initrd");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("root.tar");
    fs::write(&path, &tar).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("wrote {} ({} bytes)", path.display(), tar.len());
    Ok(path)
}

/// Builds `purg-probe` over a fresh `root/`, boots it in QEMU and compares
/// its serial output with `crates/purg-probe/expected.out`. Under TCG it
/// exits through semihosting with its status; under KVM (`--kvm`, aarch64
/// hosts with /dev/kvm), which has no semihosting, through PSCI
/// `SYSTEM_OFF` (`-append exit=psci`), so QEMU's status is 0 either way and
/// pass/fail comes from the serial output alone.
fn probe(kvm: bool) -> Result<()> {
    if kvm && (env::consts::ARCH != "aarch64" || !Path::new("/dev/kvm").exists()) {
        eprintln!("probe: skipping the KVM run: needs an aarch64 host with /dev/kvm");
        return Ok(());
    }
    build_root()?;
    let ws = workspace();
    run(cargo().current_dir(&ws).args(["build", "-p", "purg-emu"]))?;
    run(align::image_cargo(&ws, false)?.args([
        "build",
        "-p",
        "purg-probe",
        "--target",
        PROBE_TARGET,
        "--profile",
        "probe",
    ]))?;
    let elf = ws
        .join("target")
        .join(PROBE_TARGET)
        .join("probe/purg-probe");
    align::check_image(&ws, &elf)?;
    let bytes = fs::read(&elf).map_err(|e| format!("{}: {e}", elf.display()))?;
    let image = elf.with_extension("img");
    fs::write(&image, elf_to_image(&bytes)?).map_err(|e| format!("{}: {e}", image.display()))?;
    let fwd = free_port()?;
    let net = net_args(fwd, None);
    let mut args: Vec<&str> = if kvm {
        vec!["-accel", "kvm", "-cpu", "host", "-M", "virt,gic-version=3"]
    } else {
        vec!["-M", "virt", "-cpu", "cortex-a72"]
    };
    args.extend(["-m", "512M", "-nographic"]);
    if kvm {
        args.extend(["-append", "exit=psci"]);
    } else {
        args.push("-semihosting");
    }
    args.extend(net.iter().map(String::as_str));
    args.push("-kernel");
    eprintln!("probe: qemu-system-aarch64 {}", args.join(" "));
    let mut mounted = false;
    let r = run_qemu(&args, &image, PROBE_TIMEOUT, &[], |text| {
        if !mounted && text.contains("probe: net ready\n") {
            mounted = true;
            mount_guest(&ws, fwd, "probe")?;
        }
        Ok(())
    })?;
    // The stamped console is kept for a run that was killed too.
    phases::report(&ws, "probe", if kvm { "kvm" } else { "tcg" }, &r.lines);
    let r = r.finished("probe")?;
    show_console(&r.serial);
    let golden = ws.join("crates/purg-probe/expected.out");
    let want = fs::read_to_string(&golden).map_err(|e| format!("{}: {e}", golden.display()))?;
    let want = expand_golden(&want, "probe", &ws, None)?;
    if let Some(p) = r.exit_problem() {
        return Err(format!("probe: {p}"));
    }
    if r.serial != want {
        return Err(format!(
            "probe output differs from {} ({}); expected:\n{want}",
            golden.display(),
            first_difference(&r.serial, &want)
        ));
    }
    if !mounted {
        return Err(String::from("probe finished without announcing tcp!*!6666"));
    }
    eprintln!(
        "probe: {} passed in {:.2}s",
        if kvm { "kvm" } else { "tcg" },
        r.elapsed.as_secs_f64()
    );
    Ok(())
}

/// The sh corpus: the `.sh` files in `root/lib/test/sh`, sorted, as the
/// probe and the kernel run them.
const SH_CORPUS: &str = "lib/test/sh";

/// Expands a golden file's `@sh-corpus` line into the lines the probe or
/// kernel prints for each corpus script (`<prefix>: sh /lib/test/sh/x.sh`
/// then `<prefix>: ok`), or for those named in `only`. Listing the
/// scripts here rather than in each golden file means a new script
/// changes no golden file, so branches that add scripts do not conflict.
fn expand_golden(text: &str, prefix: &str, ws: &Path, only: Option<&[&str]>) -> Result<String> {
    let dir = ws.join("root").join(SH_CORPUS);
    let mut scripts: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".sh"))
        .filter(|n| only.is_none_or(|o| o.iter().any(|s| n.strip_suffix(".sh") == Some(*s))))
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
/// takes about as long as the probe: 170 s on a CI runner, and up to
/// 300 s on a loaded machine with the runs booting side by side.
/// Generous, so a slow runner does not fail a good run.
const KERNEL_TIMEOUT: Duration = Duration::from_secs(600);

/// A smoke run takes under a minute.
const KERNEL_SMOKE_TIMEOUT: Duration = Duration::from_secs(180);

impl KernelOpts {
    /// A timeout for a run with `--smp` CPUs: `one`, the timeout with one
    /// CPU, and half as long again for each two more. Under TCG every vCPU
    /// is a host thread, so on a busy host a run with four CPUs has taken
    /// up to 2.8 times as long as one with one.
    fn scale(&self, one: Duration) -> Duration {
        one * (2 + self.smp as u32 / 2) / 2
    }
}

/// One QEMU configuration `xtask kernel` boots: a name, the machine
/// arguments before `-kernel`, and the exception level the kernel must
/// report booting at.
struct KernelRun {
    name: &'static str,
    args: &'static [&'static str],
    el: u32,
    /// Boot with `-initrd root.tar` instead of the compiled-in root.
    initrd: bool,
    /// Boot a second time on the same disk, to check what the first left.
    disk_reboot: bool,
    /// Run only the [`KERNEL_SMOKE`] scripts of the sh corpus.
    smoke: bool,
}

/// The sh scripts the smoke runs keep: timers and sleeps, kills of
/// progs and process groups, pipes and streams, file2chan, and a
/// network mount, the paths where interrupts and the scheduler meet.
/// Every script runs on the other configurations.
const KERNEL_SMOKE: &[&str] = &["jobs", "m3", "nsnet", "pgrp", "pipes", "stream"];

/// The TCG runs: GICv2 and GICv3 at EL1, and GICv3 with EL2
/// (`virtualization=on`), which exercises the EL2-to-EL1 path, on a
/// Cortex-A72 and on `-cpu max` (every feature QEMU has, to catch
/// assumptions about ID registers). They exit through semihosting. The
/// whole sh corpus runs on GICv2 and on `-cpu max` with GICv3, which are
/// listed first so that they start first; the other two are there for
/// their boot paths and run [`KERNEL_SMOKE`].
const KERNEL_TCG_RUNS: &[KernelRun] = &[
    KernelRun {
        name: "tcg cortex-a72 gicv2 initrd",
        args: &["-M", "virt,gic-version=2", "-cpu", "cortex-a72"],
        el: 1,
        initrd: true,
        disk_reboot: true,
        smoke: false,
    },
    KernelRun {
        name: "tcg max gicv3 el2",
        args: &["-M", "virt,gic-version=3,virtualization=on", "-cpu", "max"],
        el: 2,
        initrd: false,
        disk_reboot: false,
        smoke: false,
    },
    KernelRun {
        name: "tcg cortex-a72 gicv3",
        args: &["-M", "virt,gic-version=3", "-cpu", "cortex-a72"],
        el: 1,
        initrd: false,
        disk_reboot: false,
        smoke: true,
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
        disk_reboot: false,
        smoke: true,
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
    disk_reboot: true,
    smoke: false,
};

/// What the second boot on a run's disk (`diskcheck`) must print, besides
/// a last line of `kernel: ok` and no failures.
const DISK_REBOOT_LINES: &[&str] = &[
    "kernel: run cat /n/disk/persist.txt",
    "written by the first boot",
    "kernel: run cat /n/disk/kept.txt",
    "written by the kernel",
    "kernel: disk: check it",
];

/// Boots run `k` again on its disk (`disk_args`), with `diskcheck`: the
/// kernel mounts the file system and checks that it holds what the first
/// boot left, which a first boot cannot do across its own reboot.
fn disk_reboot(
    k: &KernelRun,
    opts: &KernelOpts,
    image: &Path,
    initrd: &str,
    disk_args: &[String],
) -> Result<()> {
    let kvm = opts.kvm;
    let smp = opts.smp.to_string();
    let mut args: Vec<&str> = k.args.to_vec();
    args.extend(["-m", &opts.mem, "-smp", &smp, "-nographic"]);
    if k.initrd {
        args.extend(["-initrd", initrd]);
    }
    if !kvm {
        args.push("-semihosting");
    }
    args.extend([
        "-append",
        if kvm {
            "exit=psci diskcheck"
        } else {
            "diskcheck"
        },
    ]);
    args.extend(disk_args.iter().map(String::as_str));
    args.push("-kernel");
    eprintln!(
        "kernel: {}: second boot: qemu-system-aarch64 {}",
        k.name,
        args.join(" ")
    );
    let r = run_qemu(&args, image, opts.scale(KERNEL_SMOKE_TIMEOUT), &[], |_| {
        Ok(())
    })?;
    let lines: Vec<&str> = r.serial.lines().collect();
    let problem = if let Some(e) = &r.killed {
        Some(e.clone())
    } else if let Some(p) = r.exit_problem() {
        Some(p)
    } else if let Some(l) = lines.iter().find(|l| l.starts_with("kernel: FAIL")) {
        Some(String::from(*l))
    } else if lines.last() != Some(&"kernel: ok") {
        Some(String::from("no final `kernel: ok`"))
    } else {
        DISK_REBOOT_LINES
            .iter()
            .find(|want| !lines.contains(want))
            .map(|want| format!("no `{want}` line"))
    };
    match problem {
        // The first boot's console comes first in the run's report.
        Some(p) => Err(format!(
            "the second boot: {p}; its console:\n{}",
            r.serial.trim_end()
        )),
        None => {
            eprintln!(
                "kernel: {}: second boot passed in {:.2}s",
                k.name,
                r.elapsed.as_secs_f64()
            );
            Ok(())
        }
    }
}

/// Boots run `k`'s machine once in the kernel's stress mode
/// (`stress=SECS`, see `crates/purg-kernel/src/stress.rs`), with no
/// network, host directory or disk, printing the serial console as it
/// comes. Passes if the kernel ends with `kernel: ok` and no failure.
fn kernel_stress(
    opts: &KernelOpts,
    k: &KernelRun,
    secs: u64,
    image: &Path,
    initrd: &str,
) -> Result<()> {
    let smp = opts.smp.to_string();
    let bootargs = format!(
        "{}stress={secs}{}",
        if opts.kvm { "exit=psci " } else { "" },
        if env::var_os("KERNEL_MEMSTAT").is_some() {
            " memstat"
        } else {
            ""
        }
    );
    let mut args: Vec<&str> = k.args.to_vec();
    args.extend(["-m", &opts.mem, "-smp", &smp, "-nographic"]);
    args.extend(["-initrd", initrd]);
    if !opts.kvm {
        args.push("-semihosting");
    }
    args.extend(["-append", &bootargs, "-kernel"]);
    eprintln!("kernel: stress: qemu-system-aarch64 {}", args.join(" "));
    // The boot and the self-tests, the run, and the watchdog's grace.
    let timeout = Duration::from_secs(secs) + opts.scale(KERNEL_TIMEOUT);
    let mut shown = 0;
    let r = run_qemu(&args, image, timeout, &[], |text| {
        if let Some(end) = text.rfind('\n')
            && end + 1 > shown
        {
            eprint!("{}", &text[shown..end + 1]);
            shown = end + 1;
        }
        Ok(())
    })?;
    // The console was shown as it came, less a last unfinished line.
    if let Some(tail) = r.serial.get(shown..).filter(|t| !t.is_empty()) {
        eprintln!("{tail}");
    }
    for l in r.stderr.lines() {
        eprintln!("kernel: stress: qemu: {l}");
    }
    if let Some(e) = &r.killed {
        return Err(format!("kernel: stress: {e}"));
    }
    let lines: Vec<&str> = r.serial.lines().collect();
    if let Some(l) = lines.iter().find(|l| l.contains("kernel: FAIL")) {
        return Err(format!("kernel: stress: {l}"));
    }
    if let Some(p) = r.exit_problem() {
        return Err(format!("kernel: stress: {p}"));
    }
    if lines.last() != Some(&"kernel: ok") {
        return Err(String::from("kernel: stress: no final `kernel: ok`"));
    }
    eprintln!("kernel: stress: passed in {:.0}s", r.elapsed.as_secs_f64());
    Ok(())
}

/// Turns the kernel's or the probe's ELF into a raw arm64 `Image`: each loadable segment
/// copied to its physical address, relative to the lowest one. The image
/// starts with the arm64 Image header (see `purg_aarch64::boot`), so QEMU
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

/// Options of `xtask kernel`.
struct KernelOpts {
    kvm: bool,
    /// QEMU's `-m`.
    mem: String,
    /// QEMU's `-smp`: the CPUs the kernel must bring up.
    smp: usize,
    /// `--stress SECS`: one boot in the kernel's stress mode instead.
    stress: Option<u64>,
    /// Boot only the configurations whose names contain this.
    only: Option<String>,
    /// How many times to boot them, to look for a failure that comes and
    /// goes; every round runs, and the last line counts the failed ones.
    repeat: usize,
}

impl KernelOpts {
    /// `--mem SIZE`, `--smp N`, `--stress SECS`, `--only NAME`,
    /// `--repeat N`, and the flags [`image_flags`] takes.
    fn parse(args: &[String]) -> Option<KernelOpts> {
        let mut mem = String::from("512M");
        let mut smp = 1;
        let mut stress = None;
        let (mut only, mut repeat) = (None, 1);
        let mut rest = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--mem" => mem = it.next()?.clone(),
                "--smp" => smp = it.next()?.parse().ok().filter(|&n| (1..=8).contains(&n))?,
                "--stress" => stress = Some(it.next()?.parse().ok()?),
                "--only" => only = Some(it.next()?.clone()),
                "--repeat" => repeat = it.next()?.parse().ok().filter(|&n| n > 0)?,
                _ => rest.push(a.clone()),
            }
        }
        let kvm = image_flags(&rest, true)?;
        Some(KernelOpts {
            kvm,
            mem,
            smp,
            stress,
            only,
            repeat,
        })
    }
}

/// The kernel's output as compared with the golden file: without the
/// `memstat` lines, and with the size of the heap test's large block,
/// which follows the RAM size, left out.
fn kernel_normalize(serial: &str) -> String {
    let mut out = String::new();
    for l in serial.lines() {
        if l.starts_with("kernel: mem ") || l.starts_with("kernel: smp: ") {
            continue;
        }
        match (l.strip_prefix("kernel: heap: "), l.find(" MiB block ok")) {
            (Some(_), Some(end)) => {
                let start = l[..end].rfind(' ').map_or(0, |i| i + 1);
                out.push_str(&l[..start]);
                out.push('N');
                out.push_str(&l[end..]);
            }
            _ => out.push_str(l),
        }
        out.push('\n');
    }
    out
}

/// Builds `purg-kernel` and boots it under each QEMU configuration (TCG,
/// or KVM with `--kvm`), checking the exit status, the boot EL line and
/// the rest of the serial output against `crates/purg-kernel/expected.out`.
fn kernel(opts: &KernelOpts) -> Result<()> {
    let kvm = opts.kvm;
    let ws = workspace();
    let initrd = initrd()?;
    let initrd = initrd
        .to_str()
        .ok_or("initrd path is not UTF-8")?
        .to_owned();
    run(cargo().current_dir(&ws).args(["build", "-p", "purg-emu"]))?;
    run(align::image_cargo(&ws, false)?.args([
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
    align::check_image(&ws, &image)?;
    let elf = fs::read(&image).map_err(|e| format!("{}: {e}", image.display()))?;
    let image = image.with_extension("img");
    fs::write(&image, elf_to_image(&elf)?).map_err(|e| format!("{}: {e}", image.display()))?;
    let golden = ws.join("crates/purg-kernel/expected.out");
    let golden_text =
        fs::read_to_string(&golden).map_err(|e| format!("{}: {e}", golden.display()))?;
    // `KERNEL_CORPUS=full` runs the whole corpus on every configuration.
    let full = env::var("KERNEL_CORPUS").is_ok_and(|c| c == "full");
    let mut runs: Vec<&KernelRun> = if kvm {
        if env::consts::ARCH != "aarch64" || !Path::new("/dev/kvm").exists() {
            eprintln!("kernel: skipping the KVM run: needs an aarch64 host with /dev/kvm");
            return Ok(());
        }
        vec![&KERNEL_KVM_RUN]
    } else {
        KERNEL_TCG_RUNS.iter().collect()
    };
    if let Some(only) = &opts.only {
        runs.retain(|k| k.name.contains(only.as_str()));
        if runs.is_empty() {
            return Err(format!("kernel: no run is named like {only:?}"));
        }
    }
    if let Some(secs) = opts.stress {
        return kernel_stress(opts, runs[0], secs, &image, &initrd);
    }
    // `KERNEL_MEMSTAT=1 cargo xtask kernel` adds the `memstat` boot
    // argument: a `kernel: mem ...` line after each test, not compared.
    let memstat = env::var_os("KERNEL_MEMSTAT").is_some();
    // Each run has its own network peers, shared directory and disks, so
    // runs can boot at once.
    let run = |k: &KernelRun| -> KernelOutcome {
        // The network peers on this host: an emu exporting its root and a
        // UDP echo server.
        let (mut host, echo) = match (HostExport::start(&ws, k.name), UdpEcho::start()) {
            (Ok(h), Ok(e)) => (h, e),
            (h, e) => return KernelOutcome::setup(h.err().or(e.err()).unwrap_or_default()),
        };
        // The directory the kernel mounts on /n/host over virtio-9p.
        let shared = match SharedDir::create(&ws, k.name).and_then(|d| d.args().map(|a| (d, a))) {
            Ok(s) => s,
            Err(e) => return KernelOutcome::setup(e),
        };
        // The host's lethed serves the same directory read-only.
        let mut lethed = match HostLethed::start(&ws, &shared.0.path, k.name) {
            Ok(l) => l,
            Err(e) => return KernelOutcome::setup(e),
        };
        // The disks: a purgfs image on /n/disk, and a slow one.
        let disks =
            match disk::KernelDisks::create(&ws, k.name).and_then(|d| d.args().map(|a| (d, a))) {
                Ok(d) => d,
                Err(e) => return KernelOutcome::setup(e),
            };
        // `test` runs the conformance guests, the sh corpus and the
        // network checks instead of an interactive shell.
        let smoke = k.smoke && !full;
        let bootargs = format!(
            "{}test hostexport={} udpecho={} ping dhcp lethed={} netexport rstyx{}{}",
            if kvm { "exit=psci " } else { "" },
            host.port,
            echo.port,
            lethed.port,
            if memstat { " memstat" } else { "" },
            if smoke {
                format!(" sh={}", KERNEL_SMOKE.join(","))
            } else {
                String::new()
            }
        );
        let want = match expand_golden(&golden_text, "kernel", &ws, smoke.then_some(KERNEL_SMOKE)) {
            Ok(w) => kernel_normalize(&w),
            Err(e) => return KernelOutcome::setup(e),
        };
        let smp = opts.smp.to_string();
        let mut head: Vec<&str> = k.args.to_vec();
        head.extend(["-m", &opts.mem, "-smp", &smp, "-nographic"]);
        if k.initrd {
            head.extend(["-initrd", &initrd]);
        }
        if !kvm {
            head.push("-semihosting");
        }
        head.extend(["-append", &bootargs]);
        let timeout = opts.scale(if smoke {
            KERNEL_SMOKE_TIMEOUT
        } else {
            KERNEL_TIMEOUT
        });
        let mut refused = 0;
        let (r, mounted, mcp) = loop {
            // The ports QEMU forwards to the kernel's export and its
            // rstyxd, asked for after every helper has its own port and
            // held until QEMU starts. Something else can still take one
            // before QEMU binds it; QEMU then exits at once, and the run
            // starts again on new ports.
            let ((fwd_held, fwd), (rstyx_held, rstyx)) =
                match held_port().and_then(|f| Ok((f, held_port()?))) {
                    Ok(p) => p,
                    Err(e) => return KernelOutcome::setup(e),
                };
            let net = net_args(fwd, Some(rstyx));
            let mut args = head.clone();
            args.extend(net.iter().map(String::as_str));
            args.extend(shared.1.iter().map(String::as_str));
            args.extend(disks.1.iter().map(String::as_str));
            args.push("-kernel");
            eprintln!("kernel: {}: qemu-system-aarch64 {}", k.name, args.join(" "));
            let (mut mounted, mut mcp) = (false, false);
            drop((fwd_held, rstyx_held));
            let r = run_qemu(&args, &image, timeout, KERNEL_INPUT, |text| {
                if !mounted && text.contains("kernel: net ready\n") {
                    mounted = true;
                    mount_guest(&ws, fwd, "kernel")
                        .map_err(|e| format!("the host's mount of the kernel's export: {e}"))?;
                }
                if !mcp && text.contains("kernel: rstyx ready\n") {
                    mcp = true;
                    mcp_guest(&ws, rstyx, k.name)
                        .map_err(|e| format!("the host's MCP call: {e}"))?;
                }
                Ok(())
            });
            match r {
                Ok(r) if r.hostfwd_refused() && refused < HOSTFWD_RETRIES => {
                    refused += 1;
                    eprintln!(
                        "kernel: {}: starting again on new ports ({refused} of {HOSTFWD_RETRIES}); QEMU said: {}",
                        k.name,
                        r.stderr.trim()
                    );
                }
                r => break (r, mounted, mcp),
            }
        };
        // A helper that died under the kernel explains its failed checks.
        let helpers: Vec<String> = [host.died(), lethed.died()].into_iter().flatten().collect();
        drop(host);
        drop(lethed);
        let echoed = echo.stop();
        let r = match r {
            Ok(r) => r,
            Err(e) => return KernelOutcome::setup(e),
        };
        phases::report(&ws, "kernel", k.name, &r.lines);
        let el_line = format!("kernel: booted at EL{}", k.el);
        let smp_line = format!("kernel: smp: {} cpus online", opts.smp);
        let root_line = if k.initrd {
            "kernel: root from the initrd"
        } else {
            "kernel: root compiled in"
        };
        // The echo of the typed line is checked here and is not in the
        // golden file, which would otherwise hold backspaces.
        let echo = format!("{}{KERNEL_ECHO}", KERNEL_INPUT[0].prompt);
        let echoed_typing = r.serial.contains(&echo);
        let shown = r.serial.replacen(&echo, KERNEL_INPUT[0].prompt, 1);
        let rest: String = kernel_normalize(&shown)
            .lines()
            .filter(|l| !l.starts_with("kernel: booted at EL") && !l.starts_with("kernel: root "))
            .filter(|l| !l.starts_with("kernel: mem "))
            .flat_map(|l| [l, "\n"])
            .collect();
        let problem = if let Some(e) = &r.killed {
            Some(e.clone())
        } else if let Some(p) = r.exit_problem() {
            // The kernel's last line says what it counted.
            Some(
                match r.serial.lines().rfind(|l| l.starts_with("kernel: FAILED")) {
                    Some(l) => format!("{p}; the kernel said: {l}"),
                    None if r.serial.is_empty() => p,
                    None => format!("{p}; the console has no `kernel: FAILED` line"),
                },
            )
        } else if !r.serial.lines().any(|l| l == el_line) {
            Some(format!("no `{el_line}` line"))
        } else if !r.serial.lines().any(|l| l == smp_line) {
            Some(format!("no `{smp_line}` line"))
        } else if !r.serial.lines().any(|l| l == root_line) {
            Some(format!("no `{root_line}` line"))
        } else if !echoed_typing {
            Some(format!(
                "the console did not echo the typed line as {KERNEL_ECHO:?}"
            ))
        } else if rest != want {
            Some(format!(
                "output differs from {} ({}); expected (less the EL line):\n{want}",
                golden.display(),
                first_difference(&rest, &want)
            ))
        } else if !mounted {
            Some(format!(
                "no `kernel: net ready`: tcp!*!{GUEST_EXPORT_PORT} was not exported"
            ))
        } else if !mcp {
            Some(format!(
                "no `kernel: rstyx ready`: tcp!*!{GUEST_RSTYX_PORT} was not served"
            ))
        } else if echoed == 0 {
            Some(String::from("the UDP echo server heard nothing"))
        } else if let Err(e) = shared.0.check() {
            Some(format!("the shared directory: {e}"))
        } else if let Err(e) = disks.0.check() {
            Some(format!("the disk: {e}"))
        } else if k.disk_reboot {
            disk_reboot(k, opts, &image, &initrd, &disks.1).err()
        } else {
            None
        };
        let problem = problem.map(|p| {
            helpers
                .iter()
                .fold(p, |p, h| format!("{p}\nkernel: {}: {h}", k.name))
        });
        KernelOutcome {
            serial: r.serial,
            qemu_stderr: r.stderr,
            verdict: problem.map_or(Ok(r.elapsed), Err),
        }
    };
    let jobs = kernel_jobs(runs.len(), opts.smp);
    // What failed, each with the first line of why.
    let mut failed = Vec::new();
    for round in 1..=opts.repeat {
        if opts.repeat > 1 {
            eprintln!("kernel: round {round} of {}", opts.repeat);
        }
        let outcomes = in_parallel(&runs, jobs, |k| run(k));
        // Each run's console, whole, then what QEMU said and the verdict.
        for (k, o) in runs.iter().zip(outcomes) {
            eprintln!(
                "kernel: {}: its console ({} lines):",
                k.name,
                o.serial.lines().count()
            );
            show_console(&o.serial);
            for l in o.qemu_stderr.lines() {
                eprintln!("kernel: {}: qemu: {l}", k.name);
            }
            match o.verdict {
                Ok(t) => eprintln!("kernel: {}: passed in {:.2}s", k.name, t.as_secs_f64()),
                Err(p) => {
                    eprintln!("kernel: {}: FAILED: {p}", k.name);
                    // The stamped console says why too: it is what CI keeps.
                    let times = phases::file(&ws, "kernel", k.name);
                    let said: String = o
                        .qemu_stderr
                        .lines()
                        .map(|l| format!("qemu: {l}\n"))
                        .collect();
                    let _ = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&times)
                        .and_then(|mut f| writeln!(f, "{said}FAILED: {p}"));
                    let mut kept = times.clone();
                    if opts.repeat > 1 {
                        // The next round writes over the file.
                        kept.set_extension(format!("round{round}.txt"));
                        let _ = fs::copy(&times, &kept);
                    }
                    eprintln!(
                        "kernel: {}: the console with arrival times: {}",
                        k.name,
                        kept.display()
                    );
                    let why = p.lines().next().unwrap_or_default();
                    failed.push(if opts.repeat > 1 {
                        format!("{} in round {round} ({why})", k.name)
                    } else {
                        format!("{} ({why})", k.name)
                    });
                }
            }
        }
    }
    if opts.repeat > 1 {
        eprintln!(
            "kernel: {} of {} runs failed in {} rounds",
            failed.len(),
            runs.len() * opts.repeat,
            opts.repeat
        );
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("kernel runs failed: {}", failed.join("; ")))
    }
}

/// A finished `xtask kernel` run.
struct KernelOutcome {
    /// Its console, whole.
    serial: String,
    /// What QEMU wrote on its standard error.
    qemu_stderr: String,
    /// The time it took, or what was wrong.
    verdict: Result<Duration>,
}

impl KernelOutcome {
    /// A run that could not start.
    fn setup(e: String) -> KernelOutcome {
        KernelOutcome {
            serial: String::new(),
            qemu_stderr: String::new(),
            verdict: Err(e),
        }
    }
}

/// Where two outputs first differ, for a failure's first line.
fn first_difference(got: &str, want: &str) -> String {
    let (mut g, mut w) = (got.lines(), want.lines());
    let mut n = 1;
    loop {
        match (g.next(), w.next()) {
            (Some(a), Some(b)) if a == b => n += 1,
            (Some(a), Some(b)) => return format!("line {n}: got {a:?}, expected {b:?}"),
            (Some(a), None) => return format!("line {n}: got {a:?} after the expected end"),
            (None, Some(b)) => return format!("line {n}: the output ends, expected {b:?}"),
            (None, None) => return String::from("line endings differ"),
        }
    }
}

/// How many `xtask kernel` runs boot at once: `KERNEL_JOBS`, or one per
/// two CPUs for each of a run's `smp` CPUs (under TCG every vCPU keeps a
/// host thread busy, and the emus beside it want some too), at least one.
/// So four CPUs and `--smp 4` boot one run at a time.
fn kernel_jobs(runs: usize, smp: usize) -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let jobs = env::var("KERNEL_JOBS")
        .ok()
        .and_then(|j| j.parse().ok())
        .unwrap_or(cpus / (2 * smp.max(1)))
        .clamp(1, runs.max(1));
    if jobs > 1 {
        eprintln!("kernel: {runs} runs, {jobs} at a time");
    }
    jobs
}

/// `f` of each of `items` on `jobs` threads; the results in order.
fn in_parallel<T: Sync, R: Send>(items: &[T], jobs: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let done: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    *done[i].lock().expect("result") = Some(f(item));
                }
            });
        }
    });
    done.into_iter()
        .map(|r| r.into_inner().expect("result").expect("every item ran"))
        .collect()
}

/// Mounts the export of the guest (`who`: the probe or the kernel) from
/// this host, through QEMU's forward of host port `port`, and reads a file
/// through it.
fn mount_guest(ws: &Path, port: u16, who: &str) -> Result<()> {
    let emu = ws.join("target/debug/emu");
    let script = format!(
        "mkdir -p /tmp/{who}-mnt; mount tcp!127.0.0.1!{port} /tmp/{who}-mnt; cat /tmp/{who}-mnt/lib/probe/hello.txt"
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
            return Err(format!("{who} export mount timed out"));
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
    if stdout.trim() != "hello from the probe root" {
        return Err(format!(
            "{who} export mount failed ({status})\n{stdout}{stderr}"
        ));
    }
    Ok(())
}

/// Drives an MCP session on the kernel from this host, as an MCP client
/// would: a hosted emu runs `rcmd tcp!127.0.0.1!port mcpd`, through
/// QEMU's forward of `port` to the kernel's `auxi/rstyxd`, authenticated
/// with the test key and encrypted. One `run` call must come back with
/// its output.
fn mcp_guest(ws: &Path, port: u16, run: &str) -> Result<()> {
    let emu = ws.join("target/debug/emu");
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"xtask","version":"0"},"io.modelcontextprotocol/clientCapabilities":{}}"#;
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"run","arguments":{{"command":"echo session on the kernel; cat /dev/sysname"}},{meta}}}}}"#
    );
    let mut child = Command::new(&emu)
        .arg("-r")
        .arg(ws.join("root"))
        .arg("--vm=wasmi")
        // The kernel's rstyxd proves itself with the public test key, and
        // its test mode trusts that key for this client: trust it back.
        .args([
            "sh",
            "-c",
            &format!("bind /lib/test/keys /lib/keys; rcmd tcp!127.0.0.1!{port} mcpd"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", emu.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // End of input ends the session once the call is answered.
        let _ = stdin.write_all(request.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("emu: {e}"))? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(String::from("the MCP session on the kernel timed out"));
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
    if !status.success() || !stdout.contains(r#""stdout":"session on the kernel\n"#) {
        return Err(format!(
            "the MCP session on the kernel failed ({status})\n{stdout}{stderr}"
        ));
    }
    eprintln!("kernel: {run}: an MCP session over virtio-net answered");
    Ok(())
}

/// A hosted emu serving its root with `styxlisten -A tcp!127.0.0.1!0
/// export /` for the kernel to mount; killed when dropped.
struct HostExport {
    child: std::process::Child,
    port: u16,
}

impl HostExport {
    /// `run` names the kernel run in what the emu prints.
    fn start(ws: &Path, run: &str) -> Result<HostExport> {
        let emu = ws.join("target/debug/emu");
        let mut child = Command::new(&emu)
            .arg("-r")
            .arg(ws.join("root"))
            .args(["styxlisten", "-A", "tcp!127.0.0.1!0", "export", "/"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", emu.display()))?;
        let label = format!("kernel: {run}: host emu");
        relay(child.stderr.take().expect("piped stderr"), label.clone());
        // The guest's fd 2 is the emu's console, its stdout.
        let out = child.stdout.take().expect("piped stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        // Reads the announced address, then keeps draining the console.
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if let Some(addr) = line.strip_prefix("styxlisten: ") {
                    let _ = tx.send(addr.to_owned());
                } else {
                    eprintln!("{label}: {line}");
                }
            }
        });
        let mut host = HostExport { child, port: 0 };
        let addr = rx.recv_timeout(Duration::from_secs(20)).map_err(|_| {
            let status = host.child.try_wait().ok().flatten();
            format!("the host emu did not announce its export (exit status {status:?})")
        })?;
        host.port = addr
            .rsplit('!')
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("styxlisten announced {addr:?}"))?;
        Ok(host)
    }

    /// What became of the emu, if it is not running any more.
    fn died(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        Some(format!(
            "the host emu exporting its root exited during the run ({status})"
        ))
    }
}

impl Drop for HostExport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A directory on this host that a kernel run shares with the guest over
/// virtio-9p (`-fsdev local` and `virtio-9p-device`, mount tag `host`),
/// with the files the kernel's checks expect. Removed when dropped.
///
/// A second directory is shared as a second device, tagged `slow`, whose
/// reads QEMU throttles to [`SLOW_9P_BPS`] bytes a second: the kernel
/// kills a prog while one of its reads waits there.
struct SharedDir {
    path: PathBuf,
    slow: PathBuf,
}

/// The slow 9P device's read rate, in bytes a second: an 8 KiB read takes
/// about a second.
const SLOW_9P_BPS: u32 = 8192;

/// What the kernel writes to `kept.txt` (see `purg-kernel`'s `host9p`).
const SHARED_WRITTEN: &str = "written by the kernel\n";
/// Files in `many/`, enough for several directory reads.
const SHARED_MANY: usize = 300;

impl SharedDir {
    fn create(ws: &Path, run: &str) -> Result<SharedDir> {
        let name: String = run
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let path = ws
            .join("target/kernel-9p")
            .join(format!("{}-{name}", std::process::id()));
        let io = |e: std::io::Error| format!("{}: {e}", path.display());
        if path.exists() {
            fs::remove_dir_all(&path).map_err(io)?;
        }
        fs::create_dir_all(path.join("sub")).map_err(io)?;
        fs::create_dir_all(path.join("many")).map_err(io)?;
        fs::write(path.join("hello.txt"), "hello from the host\n").map_err(io)?;
        fs::write(path.join("sub/inner.txt"), "inner file\n").map_err(io)?;
        for i in 0..SHARED_MANY {
            fs::write(path.join(format!("many/f{i:03}")), "").map_err(io)?;
        }
        let slow = path.with_extension("slow");
        let io = |e: std::io::Error| format!("{}: {e}", slow.display());
        if slow.exists() {
            fs::remove_dir_all(&slow).map_err(io)?;
        }
        fs::create_dir_all(&slow).map_err(io)?;
        fs::write(slow.join("big.bin"), vec![b'b'; 64 << 10]).map_err(io)?;
        fs::write(slow.join("small.txt"), "small file\n").map_err(io)?;
        Ok(SharedDir { path, slow })
    }

    fn args(&self) -> Result<Vec<String>> {
        let utf8 = |p: &Path| -> Result<String> {
            let p = p.to_str().ok_or("shared directory is not UTF-8")?;
            if p.contains(',') {
                return Err(format!("{p}: QEMU options cannot hold a comma"));
            }
            Ok(String::from(p))
        };
        let (path, slow) = (utf8(&self.path)?, utf8(&self.slow)?);
        Ok([
            "-fsdev",
            &format!("local,id=host,path={path},security_model=none"),
            "-device",
            "virtio-9p-device,fsdev=host,mount_tag=host",
            "-fsdev",
            &format!(
                "local,id=slow,path={slow},security_model=none,throttling.bps-total={SLOW_9P_BPS}"
            ),
            "-device",
            "virtio-9p-device,fsdev=slow,mount_tag=slow",
        ]
        .map(String::from)
        .to_vec())
    }

    /// Checks what the kernel left: `kept.txt` written, renamed and made
    /// mode 600, `newdir` made, and the files it removed or renamed gone.
    fn check(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let p = |n: &str| self.path.join(n);
        let kept = fs::read_to_string(p("kept.txt")).map_err(|e| format!("kept.txt: {e}"))?;
        if kept != SHARED_WRITTEN {
            return Err(format!("kept.txt holds {kept:?}, not {SHARED_WRITTEN:?}"));
        }
        let mode = fs::metadata(p("kept.txt"))
            .map_err(|e| format!("kept.txt: {e}"))?
            .permissions()
            .mode();
        if mode & 0o777 != 0o600 {
            return Err(format!("kept.txt has mode {:o}, not 600", mode & 0o777));
        }
        if !p("newdir").is_dir() {
            return Err(String::from("newdir was not made"));
        }
        for gone in ["new.txt", "big.wasm", "renamed.txt"] {
            if p(gone).exists() {
                return Err(format!("{gone} is still there"));
            }
        }
        let hello = fs::read_to_string(p("hello.txt")).map_err(|e| format!("hello.txt: {e}"))?;
        if hello != "hello from the host\n" || !p("sub/inner.txt").exists() {
            return Err(String::from("hello.txt or sub/inner.txt changed"));
        }
        Ok(())
    }
}

impl Drop for SharedDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        let _ = fs::remove_dir_all(&self.slow);
    }
}

/// A UDP echo server on 127.0.0.1 for the kernel's UDP check.
struct UdpEcho {
    port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<usize>,
}

impl UdpEcho {
    fn start() -> Result<UdpEcho> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let sock = std::net::UdpSocket::bind(("127.0.0.1", 0)).map_err(|e| format!("udp: {e}"))?;
        let port = sock.local_addr().map_err(|e| format!("udp: {e}"))?.port();
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| format!("udp: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut echoed = 0;
            let mut buf = [0u8; 2048];
            while !stopped.load(Ordering::Acquire) {
                if let Ok((n, from)) = sock.recv_from(&mut buf)
                    && sock.send_to(&buf[..n], from).is_ok()
                {
                    echoed += 1;
                }
            }
            echoed
        });
        Ok(UdpEcho { port, stop, thread })
    }

    /// Stops the server; returns how many datagrams it echoed.
    fn stop(self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.thread.join().unwrap_or(0)
    }
}

/// Builds the kernel over the `root/` that is there and prints the size
/// report (see [`size`]).
fn size_report(args: &[String]) -> Result<()> {
    let (mut each, mut small) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.as_slice()) {
            ("--guests", _) => each = true,
            ("--small", _) => small = true,
            // Report on a kernel built earlier instead.
            ("--elf", [path, ..]) => return size::kernel(Path::new(path)),
            _ => {
                return Err(String::from(
                    "usage: cargo xtask size [--guests] [--small] | size --elf FILE",
                ));
            }
        }
    }
    let ws = workspace();
    let profile = if small { "kernel-small" } else { "kernel" };
    let mut build = align::image_cargo(&ws, false)?;
    build.args([
        "build",
        "--quiet",
        "-p",
        "purg-kernel",
        "--target",
        PROBE_TARGET,
        "--profile",
        profile,
    ]);
    if small {
        build.arg("--no-default-features");
        println!(
            "kernel built with --profile kernel-small --no-default-features: \
             the interpreter for size too, and no tls, net or p9"
        );
    }
    run(&mut build)?;
    size::kernel(
        &ws.join("target")
            .join(PROBE_TARGET)
            .join(profile)
            .join("purg-kernel"),
    )?;
    size::guests(&ws.join("root"), each)
}

/// Builds the experimental guests in `appl/small` into `target/small` and
/// prints how they compare with the commands `build-root` installed.
fn small() -> Result<()> {
    let ws = workspace();
    let target = ws.join("target/small");
    for (triple, packages) in SMALL_BUILDS {
        let mut cmd = cargo();
        cmd.current_dir(ws.join("appl/small"))
            .args(["build", "--release", "--target", triple])
            .env("CARGO_TARGET_DIR", &target)
            // The `std` module gets the guests' stack size; the `no_std`
            // ones set their own in their build scripts.
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                if *triple == GUEST_TARGET {
                    guest_rustflags(&ws)?
                } else {
                    reproducible_rustflags(&ws)
                },
            );
        for p in *packages {
            cmd.args(["-p", p]);
        }
        run(&mut cmd)?;
    }
    size::small(
        &ws.join("root"),
        &target.join("wasm32-unknown-unknown/release"),
        &target.join(GUEST_TARGET).join("release"),
    )
}

fn check_wasm(files: &[String]) -> Result<()> {
    for f in files {
        let bytes = fs::read(f).map_err(|e| format!("{f}: {e}"))?;
        wasm::check(&bytes, false).map_err(|e| format!("{f}: {e}"))?;
        println!("{f}: ok");
    }
    Ok(())
}

/// `sign <module> --key <seed file> [--cert <file>]... [-o <out>]`: signs
/// a module (`docs/signing.md`) with a 32-byte seed file. Each `--cert`
/// adds a certificate to the chain, first the one for the key. Signing is
/// a step after the build: `build-root` never signs, and the same module
/// and key give the same bytes on every host.
fn sign(args: &[String]) -> Result<()> {
    const USAGE: &str =
        "usage: cargo xtask sign <module> --key <seed file> [--cert <file>]... [-o <out>]";
    let (mut module, mut key, mut out, mut certs) = (None, None, None, Vec::new());
    let mut args = args.iter();
    while let Some(a) = args.next() {
        let slot = match a.as_str() {
            "--key" => &mut key,
            "-o" => &mut out,
            "--cert" => {
                certs.push(args.next().ok_or(USAGE)?.clone());
                continue;
            }
            _ if module.is_none() && !a.starts_with('-') => {
                module = Some(a.clone());
                continue;
            }
            _ => return Err(USAGE.into()),
        };
        *slot = Some(args.next().ok_or(USAGE)?.clone());
    }
    let (module, key) = (module.ok_or(USAGE)?, key.ok_or(USAGE)?);
    let read = |f: &str| fs::read(f).map_err(|e| format!("{f}: {e}"));
    let seed: [u8; purg_auth::SEED] = read(&key)?
        .try_into()
        .map_err(|_| format!("{key}: not a 32-byte seed"))?;
    let mut chain = Vec::new();
    for c in &certs {
        chain.push(purg_auth::keys::find_cert(&read(c)?).ok_or(format!("{c}: bad certificate"))?);
    }
    let signed = purg_auth::modsig::sign(&read(&module)?, &seed, chain)
        .map_err(|e| format!("{module}: {e}"))?;
    let out = out.unwrap_or_else(|| module.clone());
    fs::write(&out, &signed).map_err(|e| format!("{out}: {e}"))?;
    let digest = purg_auth::modsig::digest(&signed).map_err(|_| format!("{out}: not a module"))?;
    println!(
        "{out}: digest {} signed by {}",
        purg_auth::hex::encode(&digest),
        purg_auth::hex::encode(&purg_auth::public_key(&seed))
    );
    Ok(())
}

/// Set when xtask runs as cargo's `RUSTC_WRAPPER` for guest builds.
const WRAPPER_ENV: &str = "PURG_XTASK_RUSTC_WRAPPER";

/// Parses the flags of `probe`, `kernel` and `initrd`: `--kvm` (where
/// `kvm_ok`) and `--names`, in any order. `--names` sets
/// [`tar::NAMES_ENV`] for this process and the builds it starts, whose
/// build scripts read it, and is otherwise cleared so that an image never
/// carries the names by accident. Returns whether `--kvm` was given, or
/// `None` for anything else.
fn image_flags(args: &[String], kvm_ok: bool) -> Option<bool> {
    let (mut kvm, mut names) = (false, false);
    for a in args {
        match a.as_str() {
            "--kvm" if kvm_ok && !kvm => kvm = true,
            "--names" if !names => names = true,
            _ => return None,
        }
    }
    // SAFETY: called from `main` before any thread is started.
    unsafe {
        if names {
            env::set_var(tar::NAMES_ENV, "1");
        } else {
            env::remove_var(tar::NAMES_ENV);
        }
    }
    Some(kvm)
}

/// Runs rustc, for crates built for the guest target, with every
/// `-C metadata=<hash>` replaced by one derived from the crate's name and
/// version alone. On stable, Cargo's hash covers `rustc -vV`, including
/// the host triple; rustc orders generic instantiations by symbol name,
/// which contains that hash, so the build host would otherwise change
/// function order (and, through it, inlining) in guest modules.
///
/// This covers every crate in a guest, the registry's as well as the
/// workspace's: a dependency's hash is in the symbols of whatever is
/// instantiated from its generics. Host crates (build scripts, proc
/// macros) are left alone.
fn rustc_wrapper(args: &[String]) -> ExitCode {
    let Some((rustc, rest)) = args.split_first() else {
        eprintln!("xtask: rustc wrapper called without rustc");
        return ExitCode::FAILURE;
    };
    let guest = rest
        .windows(2)
        .any(|w| w[0] == "--target" && w[1] == GUEST_TARGET)
        || rest
            .iter()
            .any(|a| *a == format!("--target={GUEST_TARGET}"));
    let crate_name = rest
        .iter()
        .position(|a| a == "--crate-name")
        .and_then(|i| rest.get(i + 1))
        .filter(|_| guest)
        .map(|name| match env::var("CARGO_PKG_VERSION") {
            Ok(v) if !v.is_empty() => format!("{name}-{v}"),
            _ => name.clone(),
        });
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
    if env::var_os(align::WRAPPER_ENV).is_some() {
        return align::rustc_wrapper(&args);
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
        Some("lethed") => {
            let lethed_args = match args.iter().position(|a| a == "--") {
                Some(i) => args[i + 1..].to_vec(),
                None => rest,
            };
            run_lethed(&lethed_args)
        }
        Some("lethe-perf") => letheperf::cmd(&rest).map(|()| ExitCode::SUCCESS),
        Some("initrd") => match image_flags(&rest, false) {
            Some(_) => initrd().map(|_| ExitCode::SUCCESS),
            None => Err(String::from("usage: cargo xtask initrd [--names]")),
        },
        Some("diskimg") => disk::cmd(&rest).map(|()| ExitCode::SUCCESS),
        Some("check-wasm") => check_wasm(&rest).map(|()| ExitCode::SUCCESS),
        Some("sign") => sign(&rest).map(|()| ExitCode::SUCCESS),
        Some("probe") => match image_flags(&rest, true) {
            Some(kvm) => probe(kvm).map(|()| ExitCode::SUCCESS),
            None => Err(String::from("usage: cargo xtask probe [--kvm] [--names]")),
        },
        Some("kernel") => match KernelOpts::parse(&rest) {
            Some(o) => kernel(&o).map(|()| ExitCode::SUCCESS),
            None => Err(String::from(
                "usage: cargo xtask kernel [--kvm] [--names] [--mem SIZE] [--smp N] [--stress SECS] [--only NAME] [--repeat N]",
            )),
        },
        Some("clippy-guests") => clippy_guests().map(|()| ExitCode::SUCCESS),
        Some("size") => size_report(&rest).map(|()| ExitCode::SUCCESS),
        Some("small") => small().map(|()| ExitCode::SUCCESS),
        Some("bench") => bench::cmd(&rest).map(|()| ExitCode::SUCCESS),
        _ => Err(String::from(
            "usage: cargo xtask build-root | run-emu [-- emu args] | lethed [-- lethed args] | lethe-perf [--reps N] [--parts ...] | probe [--kvm] [--names] | kernel [--kvm] [--names] [--mem SIZE] [--smp N] [--stress SECS] [--only NAME] [--repeat N] | initrd [--names] | diskimg [--size SIZE] [--bsize N] OUT [DIR] | size [--guests] [--small] | small | bench [--emu] [--kernel] [--kvm] [--tcg] [--vm=...] [--rounds N] | clippy-guests | check-wasm file.wasm... | sign module --key seed",
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
