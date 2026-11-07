//! Links the bare-metal image at QEMU virt's RAM base and points the
//! embedded root at `root/`, whose guest binaries `cargo xtask build-root`
//! installs.

use std::env;
use std::path::PathBuf;

/// Files embedded in the probe's read-only root, relative to `root/`.
const EMBEDDED: &[&str] = &[
    "bin/echo.wasm",
    "bin/cat.wasm",
    "bin/test/abi.wasm",
    "lib/probe/hello.txt",
];

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = dir.join("../../root").canonicalize().unwrap_or_else(|e| {
        panic!("root/: {e}");
    });
    for f in EMBEDDED {
        let p = root.join(f);
        if !p.is_file() {
            panic!(
                "{} is missing; run `cargo xtask build-root` first",
                p.display()
            );
        }
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rustc-env=PURG_PROBE_ROOT={}", root.display());
    println!("cargo:rerun-if-changed={}", dir.join("link.x").display());
    println!(
        "cargo:rustc-link-arg-bins=-T{}",
        dir.join("link.x").display()
    );
}
