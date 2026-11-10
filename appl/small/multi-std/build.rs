//! Turns the sources of some `appl/cmd` commands into modules of one
//! binary: each `main.rs` is copied without its crate docs and its
//! `#[purgatorio::main]`, with `fn main` renamed `cmd_main`, and a
//! `dispatch` function calls the one named.

use std::env;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

/// The commands: small ones whose only dependency is the SDK.
const COMMANDS: &[&str] = &[
    "basename",
    "cat",
    "cleanname",
    "cmp",
    "cp",
    "date",
    "echo",
    "ls",
    "mkdir",
    "mv",
    "pwd",
    "rm",
    "sleep",
    "tee",
    "touch",
    "wc",
];

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut mods = String::new();
    let mut arms = String::new();
    for name in COMMANDS {
        let src = dir.join(format!("../../cmd/{name}/src/main.rs"));
        println!("cargo:rerun-if-changed={}", src.display());
        let text = fs::read_to_string(&src).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
        let body: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with("//!") && l.trim() != "#[purgatorio::main]")
            .collect();
        let body = body.join("\n").replacen("fn main(", "pub fn cmd_main(", 1);
        let file = out.join(format!("{name}.rs"));
        fs::write(&file, body).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        writeln!(
            mods,
            "pub mod {name} {{ include!(concat!(env!(\"OUT_DIR\"), \"/{name}.rs\")); }}"
        )
        .unwrap();
        writeln!(arms, "        {name:?} => {name}::cmd_main(ctx, args),").unwrap();
    }
    let cmds = format!(
        "{mods}\npub fn dispatch(name: &str, ctx: purgatorio::Context, args: Vec<String>) \
         -> Result<(), purgatorio::Error> {{\n    match name {{\n{arms}        \
         _ => Err(purgatorio::Error::from(\"multi-std: not one of my commands\")),\n    }}\n}}\n"
    );
    fs::write(out.join("cmds.rs"), cmds).unwrap();
}
