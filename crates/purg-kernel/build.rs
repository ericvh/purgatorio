//! Links the bare-metal kernel with `link.x`: loaded at QEMU virt's RAM
//! base plus 2 MiB, linked in the high half.

use std::env;
use std::path::PathBuf;

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
}
