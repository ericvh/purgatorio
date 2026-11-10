//! stack [-v] pid ...: print the call stack of a prog, the innermost
//! function first, as Inferno's `stack` does for a Dis prog. `-v` (Inferno's
//! `stackv`) adds each function's index and the module it is in.
//!
//! The stack comes from `/prog/<pid>/stack` (`docs/debug.md`), which a prog
//! has when it was started under tracing: by a prog that had `trace on`
//! written to its `ctl`, or by a descendant of one. For a running prog it
//! is the stack at its last system call or check point. A traced prog
//! that trapped stays broken until it is killed; its stack is the one it
//! trapped with, and the trap is printed first, as `exception <trap>`.
//!
//! A function is named from the `.names` file beside its module
//! (`/bin/cat.names` for `/bin/cat.wasm`; `cargo xtask build-root` writes
//! them), or from the module's own `name` section, or from its exports.
//! A function with no name prints as `func <index>`.

use std::collections::BTreeMap;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use wasmobj::{Kind, Names};

const USAGE: &str = "stack [-v] pid ...";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut verbose = false;
    while let Some(c) = arg.opt() {
        match c {
            'v' => verbose = true,
            _ => return Err(arg.usage()),
        }
    }
    let pids = arg.argv();
    if pids.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let mut failed = false;
    let mut names = BTreeMap::new();
    for pid in &pids {
        if pids.len() > 1 {
            println!("prog {pid}");
        }
        if let Err(e) = stack(pid, verbose, &mut names) {
            eprintln!("stack: {pid}: {}", e.0);
            failed = true;
        }
    }
    if failed {
        // The messages are already printed.
        purgatorio::exits("error");
    }
    Ok(())
}

/// The function names of the module at `path`.
fn module_names(path: &str) -> Names {
    if let Ok(text) = std::fs::read_to_string(wasmobj::names_path(path)) {
        return Names::from_text(&text);
    }
    let Ok(wasm) = std::fs::read(path) else {
        return Names::default();
    };
    let Ok(m) = wasmobj::parse(&wasm) else {
        return Names::default();
    };
    m.names().unwrap_or_else(|| {
        let mut text = String::new();
        for e in m.exports.iter().filter(|e| e.kind == Kind::Func) {
            text.push_str(&format!("{} {}\n", e.index, e.name));
        }
        Names::from_text(&text)
    })
}

fn stack(pid: &str, verbose: bool, names: &mut BTreeMap<String, Names>) -> Result<(), Error> {
    let read = |file: &str| {
        std::fs::read_to_string(format!("/prog/{pid}/{file}")).map_err(|_| Error::last())
    };
    let frames = read("stack")?;
    let exception = read("exception").unwrap_or_default();
    if !exception.is_empty() {
        println!("exception {}", exception.trim_end());
    }
    for line in frames.lines() {
        let fields = purgatorio::unquote_fields(line);
        let [index, path] = fields.as_slice() else {
            continue;
        };
        if index == "..." {
            println!("... {path} more");
            continue;
        }
        let Ok(index) = index.parse::<u32>() else {
            continue;
        };
        let names = names
            .entry(path.clone())
            .or_insert_with(|| module_names(path));
        let name = match names.get(index) {
            Some(n) => String::from(n),
            None => format!("func {index}"),
        };
        if verbose {
            println!("{name} {index} {path}");
        } else {
            println!("{name}");
        }
    }
    Ok(())
}
