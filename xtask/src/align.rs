//! Strict alignment for the kernel's and the probe's builds.
//!
//! `aarch64-unknown-none` is a `+strict-align` target: LLVM never emits an
//! unaligned load or store, and `ptr::read_unaligned::<u64>` becomes eight
//! byte loads. wasmi decodes its instruction stream and guest memory with
//! unaligned loads, so the whole system runs several times slower than it
//! needs to (`docs/perf.md` Section 5.1). Once the MMU is on, unaligned
//! accesses to Normal memory are allowed (`SCTLR_EL1.A` is clear).
//!
//! The code that runs with the MMU off is the exception: then all of
//! memory is Device memory and an unaligned access faults. That code is
//! `purg_boot_map` and `purg_boot_enable` in `purg-aarch64` and what they
//! call (`purg_aarch64::boot`); the rest of the boot path before the MMU
//! is on, and the secondary CPUs' path, is assembly.
//!
//! So images are built with `-C target-feature=-strict-align`, and the
//! crates the boot path can reach are built with strict alignment as
//! before: `purg-aarch64` and every crate it depends on. rustc compiles a
//! function with the target features of the crate that generates its code,
//! and fat LTO keeps them per function. A crate's code calls only its own
//! functions, those of the crates it depends on (including the generic
//! instantiations it reuses from them), and the prebuilt `core`, `alloc`
//! and `compiler_builtins`, which keep the target's strict alignment. A
//! function from a crate built without it can be inlined into a strict
//! one, and then it is compiled as part of the strict one.
//!
//! That leaves calls the compiler cannot see: an `extern` function defined
//! in a later crate, or a call through a pointer. [`check_boot`] reads the
//! linked image and follows every direct call from the two entry points,
//! and fails if one reaches a function of another crate or makes an
//! indirect call.
//!
//! The flags reach the build as `CARGO_ENCODED_RUSTFLAGS`, which takes
//! precedence over `RUSTFLAGS` and the config's `rustflags`, so the
//! environment's own flags (CI's `-D warnings`) are carried into it. The
//! per-crate flag is added by xtask itself, run as cargo's
//! `RUSTC_WRAPPER` ([`rustc_wrapper`]): cargo has no stable per-crate
//! rustflags.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use crate::size::{
    SHF_EXECINSTR, SHT_SYMTAB, STT_FUNC, cstr, sections, symbol_crate, u32_at, u64_at,
};
use crate::{PROBE_TARGET, Result, cargo};

/// Set, to the crates built with strict alignment (comma-separated crate
/// names), when xtask runs as cargo's `RUSTC_WRAPPER` for an image build.
pub const WRAPPER_ENV: &str = "PURG_XTASK_STRICT_ALIGN";

const LOOSE: &str = "-Ctarget-feature=-strict-align";
const STRICT: &str = "-Ctarget-feature=+strict-align";

/// The warning rustc prints for every crate built with either flag:
/// `strict-align` is not in its list of known features, though it passes
/// it to LLVM, which knows it. It is not a lint, so `-D warnings` leaves
/// it a warning and nothing can allow it; the wrapper drops it.
const WARNING: &str =
    "unknown and unstable feature specified for `-Ctarget-feature`: `strict-align`";

/// The prebuilt crates, which keep the target's strict alignment.
const SYSROOT: &[&str] = &["core", "alloc", "compiler_builtins"];

/// What the boot path may call by an unmangled name besides itself:
/// `compiler_builtins`' memory functions.
const MEM: &[&str] = &["memcpy", "memmove", "memset", "memcmp", "bcmp"];

/// The Rust functions `_start` calls with the MMU off.
const BOOT_ROOTS: &[&str] = &["purg_boot_map", "purg_boot_enable"];

/// A cargo command for building an image for [`PROBE_TARGET`] from `ws`:
/// without strict alignment, but for the crates the boot path can reach.
/// With `strict`, as the target defines it: strict alignment everywhere.
pub fn image_cargo(ws: &Path, strict: bool) -> Result<Command> {
    image_cargo_with(ws, strict, None)
}

/// [`image_cargo`] with `extra` rustflags (whitespace-separated) after
/// the environment's.
pub fn image_cargo_with(ws: &Path, strict: bool, extra: Option<&str>) -> Result<Command> {
    let mut cmd = cargo();
    cmd.current_dir(ws);
    if strict && extra.is_none() {
        return Ok(cmd);
    }
    let mut flags = env_rustflags();
    flags.extend(
        extra
            .unwrap_or_default()
            .split_whitespace()
            .map(String::from),
    );
    if !strict {
        let crates = strict_crates(ws)?.join(",");
        flags.push(String::from(LOOSE));
        // Cargo does not see the flag the wrapper adds, so a crate that
        // joined the strict set would not be rebuilt. A cfg naming the
        // set is in every crate's flags, which cargo does see.
        flags.push(format!("--cfg=purg_strict_align=\"{:016x}\"", fnv(&crates)));
        flags.push(String::from(
            "--check-cfg=cfg(purg_strict_align, values(any()))",
        ));
        cmd.env(WRAPPER_ENV, crates).env(
            "RUSTC_WRAPPER",
            env::current_exe().map_err(|e| format!("xtask path: {e}"))?,
        );
    }
    cmd.env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"));
    Ok(cmd)
}

/// FNV-1a of `s`.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The rustflags cargo would use from the environment: those of
/// `CARGO_ENCODED_RUSTFLAGS`, or else of `RUSTFLAGS`.
fn env_rustflags() -> Vec<String> {
    if let Ok(e) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        return e
            .split('\x1f')
            .filter(|f| !f.is_empty())
            .map(String::from)
            .collect();
    }
    env::var("RUSTFLAGS")
        .map(|r| r.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// `purg-aarch64` and every crate it depends on for [`PROBE_TARGET`],
/// with all its features, by crate name.
fn strict_crates(ws: &Path) -> Result<Vec<String>> {
    let out = cargo()
        .current_dir(ws)
        .args([
            "tree",
            "--quiet",
            "-p",
            "purg-aarch64",
            "--target",
            PROBE_TARGET,
            "-e",
            "normal",
            "--all-features",
            "--prefix",
            "none",
            "-f",
            "{lib}",
        ])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cargo tree: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo tree failed: {}", out.status));
    }
    let set: BTreeSet<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(String::from)
        .collect();
    if !set.contains("purg_aarch64") {
        return Err(String::from("cargo tree: purg_aarch64 not found"));
    }
    Ok(set.into_iter().collect())
}

/// Runs rustc for an image build: adds strict alignment for the crates in
/// [`WRAPPER_ENV`] when built for [`PROBE_TARGET`], and drops [`WARNING`]
/// from what rustc prints.
pub fn rustc_wrapper(args: &[String]) -> ExitCode {
    let Some((rustc, rest)) = args.split_first() else {
        eprintln!("xtask: rustc wrapper called without rustc");
        return ExitCode::FAILURE;
    };
    let target = rest
        .windows(2)
        .any(|w| w[0] == "--target" && w[1] == PROBE_TARGET)
        || rest
            .iter()
            .any(|a| *a == format!("--target={PROBE_TARGET}"));
    let name = rest
        .iter()
        .position(|a| a == "--crate-name")
        .and_then(|i| rest.get(i + 1));
    let strict = env::var(WRAPPER_ENV).unwrap_or_default();
    let mut cmd = Command::new(rustc);
    cmd.args(rest);
    if target && name.is_some_and(|n| strict.split(',').any(|c| c == n)) {
        // After the global flag, so it wins.
        cmd.arg(STRICT);
    }
    let mut child = match cmd.stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xtask: {rustc}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(err) = child.stderr.take() {
        let mut out = std::io::stderr().lock();
        for line in BufReader::new(err).split(b'\n') {
            let Ok(line) = line else { break };
            // Cargo reads diagnostics as one JSON object a line; the
            // warning, and rustc's count of it, are dropped whole.
            let text = String::from_utf8_lossy(&line);
            if text.contains(WARNING) || dropped_count(&text) {
                continue;
            }
            let _ = out.write_all(&line);
            let _ = out.write_all(b"\n");
        }
    }
    match child.wait() {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("xtask: {rustc}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Whether `line` is rustc's "1 warning emitted" for a crate whose one
/// warning was [`WARNING`]. Cargo counts warnings itself and ignores
/// this line, but a plain rustc invocation would print it.
fn dropped_count(line: &str) -> bool {
    line.contains("\"message\":\"1 warning emitted\"")
        || line.trim() == "warning: 1 warning emitted"
}

/// Checks that the code `_start` runs with the MMU off, in the linked
/// image `elf`, stays in crates built with strict alignment (see the
/// module docs): follows every `bl` and `b` from [`BOOT_ROOTS`], and fails
/// on a call to a function of another crate, to an address that is not a
/// function, or through a register. `strict` is the crates built with
/// strict alignment. Returns the functions reached.
pub fn check_boot(elf: &[u8], strict: &[&str]) -> Result<Vec<String>> {
    let bad = || String::from("boot check: not a 64-bit little-endian ELF");
    let secs = sections(elf).ok_or_else(bad)?;
    let symtab = secs
        .iter()
        .find(|s| s.kind == SHT_SYMTAB)
        .ok_or("boot check: the image has no symbol table")?;
    let strtab = secs.get(symtab.link).ok_or_else(bad)?.offset;
    // Functions by address; folded functions keep their first name.
    let mut funcs: BTreeMap<u64, (u64, String)> = BTreeMap::new();
    for i in 0..symtab.size / symtab.entsize.max(1) {
        let e = symtab.offset + i * symtab.entsize;
        let info = *elf.get(e + 4).ok_or_else(bad)?;
        let addr = u64_at(elf, e + 8).ok_or_else(bad)?;
        let size = u64_at(elf, e + 0x10).ok_or_else(bad)?;
        if info & 0xf != STT_FUNC || size == 0 {
            continue;
        }
        let name = u32_at(elf, e)
            .and_then(|n| cstr(elf, strtab + n as usize))
            .ok_or_else(bad)?;
        funcs.entry(addr).or_insert((size, name.to_string()));
    }
    let code = |addr: u64, size: u64| -> Option<&[u8]> {
        let s = secs.iter().find(|s| {
            s.flags & SHF_EXECINSTR != 0 && addr >= s.addr && addr + size <= s.addr + s.size as u64
        })?;
        let at = s.offset + (addr - s.addr) as usize;
        elf.get(at..at + size as usize)
    };
    let allowed = |name: &str| match symbol_crate(name) {
        Some(c) => strict.contains(&c) || SYSROOT.contains(&c),
        // At `opt-level = "z"` LLVM's machine outliner moves instruction
        // sequences that several functions share into functions of their
        // own. Such a function holds the very instructions its strict
        // caller was compiled to, so it is followed like the caller.
        None => {
            BOOT_ROOTS.contains(&name)
                || MEM.contains(&name)
                || name.starts_with("OUTLINED_FUNCTION_")
        }
    };
    let mut todo = Vec::new();
    for root in BOOT_ROOTS {
        let (&addr, _) = funcs
            .iter()
            .find(|(_, (_, n))| n == root)
            .ok_or_else(|| format!("boot check: no function {root}"))?;
        todo.push((addr, String::from(*root)));
    }
    let mut seen = BTreeSet::new();
    let mut reached = Vec::new();
    let mut errors = Vec::new();
    while let Some((addr, from)) = todo.pop() {
        if !seen.insert(addr) {
            continue;
        }
        let Some((size, name)) = funcs.get(&addr) else {
            errors.push(format!("{from} branches to {addr:#x}, not a function"));
            continue;
        };
        let shown = format!("{:#}", rustc_demangle::demangle(name));
        if !allowed(name) {
            errors.push(format!(
                "{from} calls {shown}, not built with strict alignment"
            ));
            continue;
        }
        let body = code(addr, *size).ok_or_else(|| format!("boot check: {shown}: no code"))?;
        for (i, w) in body.as_chunks::<4>().0.iter().enumerate() {
            let insn = u32::from_le_bytes(*w);
            let pc = addr + 4 * i as u64;
            if indirect_call(insn) {
                errors.push(format!("{shown} makes an indirect call at {pc:#x}"));
                continue;
            }
            let Some(target) = branch_target(insn, pc) else {
                continue;
            };
            if target >= addr && target < addr + size {
                continue;
            }
            todo.push((target, shown.clone()));
        }
        reached.push(shown);
    }
    if errors.is_empty() {
        reached.sort();
        Ok(reached)
    } else {
        Err(format!(
            "boot check: code run with the MMU off leaves the strict-alignment crates:\n  {}",
            errors.join("\n  ")
        ))
    }
}

/// Whether `insn` is BLR, or BLRAA, BLRAB, BLRAAZ or BLRABZ.
fn indirect_call(insn: u32) -> bool {
    insn & 0xffff_fc1f == 0xd63f_0000 || insn & 0xfeff_f800 == 0xd63f_0800
}

/// The target of `insn` at `pc` if it is B or BL (a 26-bit word offset).
fn branch_target(insn: u32, pc: u64) -> Option<u64> {
    if insn & 0x7c00_0000 != 0x1400_0000 {
        return None;
    }
    let off = (((insn & 0x03ff_ffff) << 6) as i32 >> 6) as i64 * 4;
    Some(pc.wrapping_add_signed(off))
}

/// [`check_boot`] on the image at `path`, built by [`image_cargo`] in
/// `ws`, printing how many functions the boot path reached.
pub fn check_image(ws: &Path, path: &Path) -> Result<()> {
    let elf = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let strict = strict_crates(ws)?;
    let strict: Vec<&str> = strict.iter().map(String::as_str).collect();
    let reached = check_boot(&elf, &strict)?;
    println!(
        "boot check: {} functions run with the MMU off, all built with strict alignment",
        reached.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches() {
        // bl .-4; b .+8; b.eq .+8; ret
        assert_eq!(branch_target(0x97ff_ffff, 0x1000), Some(0xffc));
        assert_eq!(branch_target(0x1400_0002, 0x1000), Some(0x1008));
        assert_eq!(branch_target(0x5400_0040, 0x1000), None);
        assert_eq!(branch_target(0xd65f_03c0, 0x1000), None);
        // blr x8; blraa x8, x9; blraaz x8; br x8; ret; retaa
        assert!(indirect_call(0xd63f_0100));
        assert!(indirect_call(0xd73f_0909));
        assert!(indirect_call(0xd63f_091f));
        assert!(!indirect_call(0xd61f_0100));
        assert!(!indirect_call(0xd65f_03c0));
        assert!(!indirect_call(0xd65f_0bff));
    }

    #[test]
    fn warning_counts() {
        assert!(dropped_count(
            r#"{"$message_type":"diagnostic","message":"1 warning emitted","level":"warning"}"#
        ));
        assert!(!dropped_count(r#"{"message":"2 warnings emitted"}"#));
    }
}
