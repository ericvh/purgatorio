//! `cargo xtask fonts [--check] [--vm=wasmtime|wasmi]`: makes `root/fonts`
//! from the sources in `fonts/` with the `fontgen` guest, run in the emu.
//!
//! fontgen rasterizes with floating point, and only as a wasm guest is
//! that the same on every host, so it runs under the emu rather than in
//! xtask. The emu's root is the workspace, so fontgen reads
//! `/fonts/recipe` and writes `/target/fonts-out`; the result replaces
//! `root/fonts`. Generating needs the emu built, which `build-root` does
//! not, so the output is committed and `--check` makes it again and
//! fails if any file differs, is missing or is extra. CI runs the check on
//! every host.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, build_root, cargo, run, workspace};

/// Every file under `dir`, by its path relative to `dir`, with its bytes.
fn tree(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
        let rd = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for e in rd {
            let p = e.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if p.is_dir() {
                walk(base, &p, out)?;
            } else {
                let data = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                out.insert(
                    p.strip_prefix(base).expect("under base").to_path_buf(),
                    data,
                );
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    if dir.exists() {
        walk(dir, dir, &mut out)?;
    }
    Ok(out)
}

/// Runs fontgen on `fonts/recipe` with engine `vm`; returns the output
/// directory.
pub fn generate(vm: &str) -> Result<PathBuf> {
    let ws = workspace();
    build_root()?;
    run(cargo()
        .current_dir(&ws)
        .args(["build", "--quiet", "-p", "purg-emu"]))?;
    let out = ws.join("target/fonts-out");
    if out.exists() {
        fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    run(Command::new(ws.join("target/debug/emu"))
        .arg("-r")
        .arg(&ws)
        .arg(format!("--vm={vm}"))
        // fontgen's status, as the emu's.
        .arg("-x")
        .args([
            "/root/bin/fontgen.wasm",
            "-o",
            "/target/fonts-out",
            "/fonts/recipe",
        ]))?;
    Ok(out)
}

pub fn cmd(args: &[String]) -> Result<()> {
    let mut check = false;
    let mut vm = String::from("wasmtime");
    for a in args {
        if a == "--check" {
            check = true;
        } else if let Some(v) = a.strip_prefix("--vm=") {
            vm = String::from(v);
        } else {
            return Err(String::from(
                "usage: cargo xtask fonts [--check] [--vm=wasmtime|wasmi]",
            ));
        }
    }
    let ws = workspace();
    let out = generate(&vm)?;
    let dest = ws.join("root/fonts");
    let made = tree(&out)?;
    if check {
        let have = tree(&dest)?;
        let mut bad = Vec::new();
        for (p, data) in &made {
            match have.get(p) {
                None => bad.push(format!("missing from root/fonts: {}", p.display())),
                Some(d) if d != data => bad.push(format!("differs: {}", p.display())),
                Some(_) => {}
            }
        }
        for p in have.keys().filter(|p| !made.contains_key(*p)) {
            bad.push(format!("not made by fontgen: {}", p.display()));
        }
        if !bad.is_empty() {
            return Err(format!(
                "root/fonts is not what `cargo xtask fonts` makes:\n{}",
                bad.join("\n")
            ));
        }
        eprintln!(
            "root/fonts: {} files, all as fontgen makes them",
            made.len()
        );
        return Ok(());
    }
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    for (p, data) in &made {
        let to = dest.join(p);
        fs::create_dir_all(to.parent().expect("a file in a directory"))
            .map_err(|e| format!("{}: {e}", to.display()))?;
        fs::write(&to, data).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    let bytes: usize = made.values().map(Vec::len).sum();
    eprintln!("root/fonts: {} files, {bytes} bytes", made.len());
    Ok(())
}
