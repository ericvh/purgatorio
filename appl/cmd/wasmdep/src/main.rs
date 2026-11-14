//! wasmdep file ...: print what a WebAssembly module needs from the
//! system that runs it, as Inferno's `disdep` lists the modules a Dis file
//! depends on.
//!
//! For each file it prints the ABI version the module was built for
//! (`abi -` for a module without a `purgatorio.abi` section, such as a
//! third-party WASI program), then a line `import <module> <name>` for
//! each function it imports, in import order, and `import <module> <name>
//! <kind>` for anything else it imports. A module that imports
//! `purgatorio.load` or `purgatorio.spawn_prog` can load reactors or start
//! commands: each path ending in `.wasm` found in its data is printed as
//! `refers <path>`. That is a guess from the module's strings: the names
//! a command builds at run time (as `sh` does for `load` and for every
//! command) cannot be found this way.
//!
//! With more than one file each block starts with `module <file>`.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use wasmobj::Kind;

const USAGE: &str = "wasmdep file ...";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    if arg.opt().is_some() {
        return Err(arg.usage());
    }
    let files = arg.argv();
    if files.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let mut failed = false;
    for file in &files {
        if files.len() > 1 {
            println!("module {file}");
        }
        if let Err(e) = deps(file) {
            eprintln!("wasmdep: {}", e.0);
            failed = true;
        }
    }
    if failed {
        // The messages are already printed.
        purgatorio::exits("fail:errors");
    }
    Ok(())
}

/// Prints one module's needs.
fn deps(file: &str) -> Result<(), Error> {
    let wasm = std::fs::read(file).map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
    let m = wasmobj::parse(&wasm).map_err(|e| Error(format!("{file}: {e}")))?;
    match m.custom(purgatorio::abi::ABI_SECTION) {
        Some(v) => println!("abi {}", String::from_utf8_lossy(v)),
        None => println!("abi -"),
    }
    for i in &m.imports {
        match i.ty.kind() {
            Kind::Func => println!("import {} {}", i.module, i.name),
            kind => println!("import {} {} {}", i.module, i.name, kind.as_str()),
        }
    }
    let loads = m.imports.iter().any(|i| {
        i.module == purgatorio::abi::MODULE
            && [purgatorio::abi::sys::LOAD, purgatorio::abi::sys::SPAWN_PROG].contains(&i.name)
    });
    if loads {
        for path in wasmobj::module_paths(&m) {
            println!("refers {path}");
        }
    }
    Ok(())
}
