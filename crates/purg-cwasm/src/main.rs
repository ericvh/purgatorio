//! cwasm: compiles modules ahead of time for the kernel's AOT engine
//! (`purg-vm-aot`, `docs/perf.md`).
//!
//! - `cwasm [-k native|pulley] -o SET module.wasm...` writes one image
//!   set to link into the kernel, each image under the SHA-256 digest of
//!   its module (`docs/signing.md`).
//! - `cwasm [-k native|pulley] -s SEED -r ROOT -d DIR rel.wasm...` writes
//!   a signed image for each module `ROOT/rel.wasm` as `DIR/rel.cwasm`,
//!   signed with the 32-byte Ed25519 seed in the file `SEED`, for the
//!   kernel to load at run time (`purg_vm_aot::image`).
//!
//! The target is fixed (`aarch64-unknown-none` or `pulley64`), so no
//! feature of the build host's CPU goes into the code, and Ed25519
//! signatures are deterministic: the same modules give the same bytes on
//! any host with this wasmtime.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use purg_vm_aot::{Images, Kind, Signed, config};
use wasmtime::Engine;

const USAGE: &str = "usage: cwasm [-k native|pulley] -o set module.wasm...\n       cwasm [-k native|pulley] -s seed -r root -d dir rel.wasm...";

struct Opts {
    kind: Kind,
    out: Option<String>,
    seed: Option<String>,
    root: Option<PathBuf>,
    dir: Option<PathBuf>,
    files: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        kind: Kind::Native,
        out: None,
        seed: None,
        root: None,
        dir: None,
        files: Vec::new(),
    };
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'_, String>| it.next().cloned().ok_or(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-k" => {
                o.kind = match value(&mut it)?.as_str() {
                    "native" => Kind::Native,
                    "pulley" => Kind::Pulley,
                    _ => return Err(String::from(USAGE)),
                }
            }
            "-o" => o.out = Some(value(&mut it)?),
            "-s" => o.seed = Some(value(&mut it)?),
            "-r" => o.root = Some(PathBuf::from(value(&mut it)?)),
            "-d" => o.dir = Some(PathBuf::from(value(&mut it)?)),
            _ => o.files.push(a.clone()),
        }
    }
    if o.files.is_empty() || o.out.is_some() == o.seed.is_some() {
        return Err(String::from(USAGE));
    }
    Ok(o)
}

/// Compiles `wasm`; returns its digest and image.
fn compile(engine: &Engine, f: &Path) -> Result<([u8; 32], Vec<u8>, usize), String> {
    let wasm = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
    let digest = purg_auth::modsig::inspect(&wasm)
        .map_err(|_| format!("{}: not a well-formed module", f.display()))?
        .digest;
    let image = engine
        .precompile_module(&wasm)
        .map_err(|e| format!("{}: {e}", f.display()))?;
    Ok((digest, image, wasm.len()))
}

/// Runs `job` over `items` on every CPU, keeping the results in order.
fn parallel<T: Sync, R: Send>(
    items: &[T],
    job: impl Fn(&T) -> Result<R, String> + Sync,
) -> Result<Vec<R>, String> {
    let next = Mutex::new(0usize);
    let results: Mutex<Vec<Option<Result<R, String>>>> =
        Mutex::new((0..items.len()).map(|_| None).collect());
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads.min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = {
                        let mut n = next.lock().unwrap_or_else(|e| e.into_inner());
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some(item) = items.get(i) else {
                        return;
                    };
                    let r = job(item);
                    results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(String::from("not compiled"))))
        .collect()
}

fn run(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    let engine = Engine::new(&config(o.kind)?).map_err(|e| e.to_string())?;
    if let Some(out) = &o.out {
        let files: Vec<PathBuf> = o.files.iter().map(PathBuf::from).collect();
        let compiled = parallel(&files, |f| compile(&engine, f))?;
        let wasm_bytes: usize = compiled.iter().map(|c| c.2).sum();
        let image_bytes: usize = compiled.iter().map(|c| c.1.len()).sum();
        let images: Vec<_> = compiled.into_iter().map(|(d, i, _)| (d, i)).collect();
        let set = Images::build(o.kind, &images);
        std::fs::write(out, &set).map_err(|e| format!("{out}: {e}"))?;
        eprintln!(
            "cwasm: {} modules, {wasm_bytes} bytes of wasm, {image_bytes} bytes of {} images, {} bytes in {out}",
            files.len(),
            o.kind.target(),
            set.len()
        );
        return Ok(());
    }
    let seed_file = o.seed.as_deref().ok_or(USAGE)?;
    let seed: [u8; 32] = std::fs::read(seed_file)
        .map_err(|e| format!("{seed_file}: {e}"))?
        .try_into()
        .map_err(|_| format!("{seed_file}: not a 32-byte seed"))?;
    let (root, dir) = match (&o.root, &o.dir) {
        (Some(r), Some(d)) => (r, d),
        _ => return Err(String::from(USAGE)),
    };
    let sizes = parallel(&o.files, |rel| {
        let (digest, image, wasm) = compile(&engine, &root.join(rel))?;
        let file = Signed::build(o.kind, &digest, &image, &seed, Vec::new())
            .map_err(|e| format!("{rel}: {e}"))?;
        let out = dir.join(rel).with_extension("cwasm");
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        std::fs::write(&out, &file).map_err(|e| format!("{}: {e}", out.display()))?;
        Ok((wasm, file.len()))
    })?;
    eprintln!(
        "cwasm: {} modules, {} bytes of wasm, {} bytes of signed {} images in {}",
        sizes.len(),
        sizes.iter().map(|s| s.0).sum::<usize>(),
        sizes.iter().map(|s| s.1).sum::<usize>(),
        o.kind.target(),
        dir.display()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool compiles for targets that are not the build host's: CI
    /// builds the kernel's aarch64 images on x86_64 too. Each target here
    /// is foreign to some build host.
    #[test]
    fn every_backend() {
        for t in [
            "aarch64-unknown-none",
            "x86_64-unknown-linux-gnu",
            "pulley64",
        ] {
            let mut c = wasmtime::Config::new();
            c.target(t).unwrap();
            let e = Engine::new(&c).unwrap_or_else(|e| panic!("{t}: {e}"));
            e.precompile_module(b"\0asm\x01\0\0\0")
                .unwrap_or_else(|e| panic!("{t}: {e}"));
        }
        for kind in [Kind::Native, Kind::Pulley] {
            Engine::new(&config(kind).unwrap()).unwrap();
        }
    }
}
