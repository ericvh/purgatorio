//! Gives `mash` a larger shadow stack than `xtask build-root`'s default for
//! guests (256 KiB). Its evaluator recurses on nested function calls up to
//! its own limit, and reaching that limit (`ctl.mash` does) needs between
//! 128 and 256 KiB; this leaves at least twice that. A link argument from
//! a build script comes after the default and wins. See docs/footprint.md.

fn main() {
    // Host builds of this crate (clippy, tests) have no such linker option.
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("wasm") {
        println!("cargo:rustc-link-arg-bins=-zstack-size=524288");
    }
}
