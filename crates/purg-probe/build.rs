//! Links the bare-metal image with `purg-aarch64`'s `link.x` and embeds
//! `root/bin` and `root/lib` (whose guest binaries `cargo xtask build-root`
//! installs) as the files of the probe's root tree. The guests' `.names`
//! files stay out as they do of the kernel's (`xtask/src/tar.rs`).

use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

/// `xtask/src/tar.rs` returns this.
type Result<T> = std::result::Result<T, String>;

/// Only its image filter is used here.
#[path = "../../xtask/src/tar.rs"]
#[allow(dead_code)]
mod tar;

/// Directories under `root/` embedded in the image.
const EMBEDDED: &[&str] = &["bin", "lib"];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = dir.join("../../root").canonicalize().unwrap_or_else(|e| {
        panic!("root/: {e}");
    });
    if !root.join("bin/sh.wasm").is_file() {
        panic!("root/bin is missing guest binaries; run `cargo xtask build-root` first");
    }
    let mut files = Vec::new();
    for d in EMBEDDED {
        println!("cargo:rerun-if-changed={}", root.join(d).display());
        walk(&root.join(d), &mut files);
    }
    println!("cargo:rerun-if-changed=../../xtask/src/tar.rs");
    println!("cargo:rerun-if-env-changed={}", tar::NAMES_ENV);
    let names = tar::names_wanted();
    files.retain(|f| {
        let rel = f.strip_prefix(&root).unwrap().to_str().unwrap();
        tar::in_image(&rel.replace(std::path::MAIN_SEPARATOR, "/"), names)
    });
    let mut src = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        let name = f.strip_prefix(&root).unwrap().to_str().unwrap();
        writeln!(
            src,
            "    ({:?}, include_bytes!({:?})),",
            format!("/{name}"),
            f.to_str().unwrap()
        )
        .unwrap();
    }
    src.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded.rs");
    fs::write(&out, src).unwrap();
    println!(
        "cargo:rerun-if-changed={}",
        dir.join("../purg-aarch64/link.x").display()
    );
    println!(
        "cargo:rustc-link-arg-bins=-T{}",
        dir.join("../purg-aarch64/link.x").display()
    );
}
