//! cwasm [-k native|pulley] -o SET module.wasm...: compiles modules ahead
//! of time for the kernel's experimental AOT engine (`purg-vm-aot`,
//! `docs/perf.md`) and writes them as one image set, each image under
//! the SHA-256 digest of its module (`docs/signing.md`).
//!
//! The target is fixed (`aarch64-unknown-none` or `pulley64`), so no
//! feature of the build host's CPU goes into the code: the same modules
//! give the same set on any host with this wasmtime.

use std::process::ExitCode;

use purg_vm_aot::{Images, Kind, config};
use wasmtime::Engine;

const USAGE: &str = "usage: cwasm [-k native|pulley] -o set module.wasm...";

fn run(args: &[String]) -> Result<(), String> {
    let (mut kind, mut out, mut files) = (Kind::Native, None, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-k" => {
                kind = match it.next().map(String::as_str) {
                    Some("native") => Kind::Native,
                    Some("pulley") => Kind::Pulley,
                    _ => return Err(String::from(USAGE)),
                }
            }
            "-o" => out = Some(it.next().ok_or(USAGE)?.clone()),
            _ => files.push(a.clone()),
        }
    }
    let out = out.ok_or(USAGE)?;
    if files.is_empty() {
        return Err(String::from(USAGE));
    }
    let engine = Engine::new(&config(kind)?).map_err(|e| e.to_string())?;
    let mut images = Vec::new();
    let (mut wasm_bytes, mut image_bytes) = (0, 0);
    for f in &files {
        let wasm = std::fs::read(f).map_err(|e| format!("{f}: {e}"))?;
        let digest = purg_auth::modsig::inspect(&wasm)
            .map_err(|_| format!("{f}: not a well-formed module"))?
            .digest;
        let image = engine
            .precompile_module(&wasm)
            .map_err(|e| format!("{f}: {e}"))?;
        wasm_bytes += wasm.len();
        image_bytes += image.len();
        images.push((digest, image));
    }
    let set = Images::build(kind, &images);
    std::fs::write(&out, &set).map_err(|e| format!("{out}: {e}"))?;
    eprintln!(
        "cwasm: {} modules, {wasm_bytes} bytes of wasm, {image_bytes} bytes of {} images, {} bytes in {out}",
        files.len(),
        kind.target(),
        set.len()
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cwasm: {e}");
            ExitCode::FAILURE
        }
    }
}
