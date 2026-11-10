//! Links the bare-metal kernel with `link.x` (loaded at QEMU virt's RAM base
//! plus 2 MiB, linked in the high half) and compiles in the root file
//! system, as Inferno's kernels do: a ustar archive of `root/bin` and
//! `root/lib`, written by the same deterministic writer as `cargo xtask
//! initrd`, so the compiled-in root and the initrd are byte-identical.
//!
//! If the guests have not been built (`cargo xtask build-root`), the
//! archive is empty and the kernel needs an initrd to boot. That keeps
//! `cargo clippy` working without the guests.

use std::env;
use std::fs;
use std::path::PathBuf;

/// `xtask/src/tar.rs` returns this.
type Result<T> = std::result::Result<T, String>;

#[path = "../../xtask/src/tar.rs"]
mod tar;

/// Directories of `root/` in the archive; the same as `xtask initrd`.
const ROOT_DIRS: &[&str] = &["bin", "lib"];

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed={}", dir.join("link.x").display());
    println!(
        "cargo:rustc-link-arg-bins=-T{}",
        dir.join("link.x").display()
    );

    let root = dir.join("../../root");
    println!("cargo:rerun-if-changed=../../xtask/src/tar.rs");
    for d in ROOT_DIRS {
        println!("cargo:rerun-if-changed={}", root.join(d).display());
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("root.tar");
    let archive = if root.join("bin/sh.wasm").is_file() {
        tar::archive(&root, ROOT_DIRS).unwrap_or_else(|e| panic!("root.tar: {e}"))
    } else {
        println!(
            "cargo:warning=root/bin has no guests: the kernel has no compiled-in root \
             (run `cargo xtask build-root`)"
        );
        vec![0; 1024]
    };
    fs::write(&out, archive).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
}
