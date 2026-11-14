//! wasmdump [-hsiemcf] file ...: print what is in a WebAssembly module.
//! It has the role Inferno's `disdump` has for Dis files, at the level of
//! the module's structure: function bodies are measured, not disassembled.
//!
//! With no option it prints everything but the function list:
//!
//! - `-h` the header: the file's size, its digest (`docs/signing.md`) and
//!   the ABI version in its `purgatorio.abi` section (`-` when it has none);
//! - `-s` the sections in file order, with offsets and sizes;
//! - `-i` the imports, grouped by the module they come from;
//! - `-e` the exports;
//! - `-m` the tables, memories and globals, with limits, and the start
//!   function;
//! - `-c` the custom sections: the ABI version, the function count of a
//!   `name` section, and each signature of a `purgatorio.sig` section with
//!   its key and the names in its certificates;
//! - `-f` every function with its index, type, body size and name. Names
//!   come from the module's `name` section, or from the `.names` file
//!   beside a stripped module, or from its exports.
//!
//! A file that is not a module is an error that names the offset of the
//! fault; the other files are still printed.

use purgatorio::arg::Arg;
use purgatorio::auth::fmt::{hex, modsig};
use purgatorio::{Context, Error};
use wasmobj::{ImportType, Kind, Module, Names};

const USAGE: &str = "wasmdump [-hsiemcf] file ...";

#[derive(Default, Clone, Copy)]
struct Parts {
    header: bool,
    sections: bool,
    imports: bool,
    exports: bool,
    items: bool,
    custom: bool,
    funcs: bool,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut parts = Parts::default();
    let mut chosen = false;
    while let Some(c) = arg.opt() {
        chosen = true;
        match c {
            'h' => parts.header = true,
            's' => parts.sections = true,
            'i' => parts.imports = true,
            'e' => parts.exports = true,
            'm' => parts.items = true,
            'c' => parts.custom = true,
            'f' => parts.funcs = true,
            _ => return Err(arg.usage()),
        }
    }
    if !chosen {
        parts = Parts {
            header: true,
            sections: true,
            imports: true,
            exports: true,
            items: true,
            custom: true,
            funcs: false,
        };
    }
    let files = arg.argv();
    if files.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let mut failed = None;
    for file in &files {
        if let Err(e) = dump(file, parts) {
            eprintln!("wasmdump: {}", e.0);
            failed = Some(e);
        }
    }
    match failed {
        // The message is already printed.
        Some(_) => purgatorio::exits("fail:errors"),
        None => Ok(()),
    }
}

fn dump(file: &str, parts: Parts) -> Result<(), Error> {
    let wasm = std::fs::read(file).map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
    let m = wasmobj::parse(&wasm).map_err(|e| Error(format!("{file}: {e}")))?;
    println!("module {file}");
    if parts.header {
        header(&wasm);
    }
    if parts.sections {
        println!("sections {}", m.sections.len());
        for s in &m.sections {
            let mut line = format!("\t{} offset {} size {}", s.kind(), s.offset, s.size);
            if let Some(name) = s.name {
                line.push(' ');
                line.push_str(name);
            }
            println!("{line}");
        }
    }
    if parts.imports {
        imports(&m);
    }
    if parts.exports {
        println!("exports {}", m.exports.len());
        for e in &m.exports {
            let mut line = format!("\t{} {} {}", e.kind.as_str(), e.index, e.name);
            if e.kind == Kind::Func
                && let Some(t) = m.func_type(e.index)
            {
                line.push_str(&format!(" {t}"));
            }
            println!("{line}");
        }
    }
    if parts.items {
        items(&m);
    }
    if parts.custom {
        custom(&wasm, &m);
    }
    if parts.funcs {
        funcs(file, &m);
    }
    Ok(())
}

fn header(wasm: &[u8]) {
    println!("size {}", wasm.len());
    // The module parsed, so its section list is good and it has a digest.
    if let Ok(info) = modsig::inspect(wasm) {
        println!("digest {}", hex::encode(&info.digest));
        match info.abi {
            [] => println!("abi -"),
            abi => println!("abi {}", String::from_utf8_lossy(abi)),
        }
    }
}

/// The imports under the module each comes from, in order of first use.
/// Each line has the item's index in its own index space.
fn imports(m: &Module<'_>) {
    println!("imports {}", m.imports.len());
    let mut modules: Vec<&str> = Vec::new();
    for i in &m.imports {
        if !modules.contains(&i.module) {
            modules.push(i.module);
        }
    }
    for module in modules {
        println!("\t{module}");
        let mut next = [0u32; 5];
        for i in &m.imports {
            let kind = i.ty.kind();
            let index = next[kind as usize];
            next[kind as usize] += 1;
            if i.module != module {
                continue;
            }
            let ty = match i.ty {
                ImportType::Func(t) => match m.types.get(t as usize) {
                    Some(t) => format!("{t}"),
                    None => format!("type {t}"),
                },
                ImportType::Table(t) => format!("{} {}", t.elem.as_str(), t.limits),
                ImportType::Memory(l) => format!("{l}"),
                ImportType::Global(g) => format!("{g}"),
                ImportType::Tag(t) => format!("type {t}"),
            };
            println!("\t\t{} {index} {} {ty}", kind.as_str(), i.name);
        }
    }
}

fn items(m: &Module<'_>) {
    println!(
        "functions {} imported {}",
        m.funcs.len(),
        m.imported(Kind::Func)
    );
    let base = m.imported(Kind::Table);
    for (i, t) in m.tables.iter().enumerate() {
        println!("table {} {} {}", base + i as u32, t.elem.as_str(), t.limits);
    }
    let base = m.imported(Kind::Memory);
    for (i, l) in m.memories.iter().enumerate() {
        println!("memory {} {l}", base + i as u32);
    }
    let base = m.imported(Kind::Global);
    for (i, g) in m.globals.iter().enumerate() {
        let init = match g.init {
            Some(v) => format!("{v}"),
            None => String::from("-"),
        };
        println!("global {} {} {init}", base + i as u32, g.ty);
    }
    match m.start {
        Some(f) => println!("start {f}"),
        None => println!("start -"),
    }
    println!("elements {}", m.elements);
    println!("data {}", m.data);
}

fn custom(wasm: &[u8], m: &Module<'_>) {
    for s in m.sections.iter().filter(|s| s.id == 0) {
        let name = s.name.unwrap_or("");
        match name {
            purgatorio::abi::ABI_SECTION => {
                println!("custom {name} {}", String::from_utf8_lossy(s.data));
            }
            "name" => match Names::from_section(s.data) {
                Ok(n) => println!("custom {name} functions {}", n.len()),
                Err(_) => println!("custom {name} bad"),
            },
            modsig::SECTION => match modsig::parse(s.data) {
                Some(entries) => {
                    println!("custom {name} signatures {}", entries.len());
                    for e in &entries {
                        let mut line = format!("\tsigner {}", hex::encode(e.key()));
                        for c in e.certs() {
                            line.push(' ');
                            line.push_str(&c.name);
                        }
                        println!("{line}");
                    }
                }
                None => println!("custom {name} bad"),
            },
            _ => println!("custom {name} size {}", s.data.len()),
        }
    }
    // More than one signature section is refused by the loader.
    if modsig::inspect(wasm).is_ok_and(|i| i.sigs.len() > 1) {
        println!("custom {} repeated", modsig::SECTION);
    }
}

fn funcs(file: &str, m: &Module<'_>) {
    // The module's own names, then the .names file beside it (which the
    // kernel's and the probe's images leave out), then its exports.
    let names = m.names().unwrap_or_else(|| {
        std::fs::read_to_string(wasmobj::names_path(file))
            .map(|t| Names::from_text(&t))
            .unwrap_or_else(|_| {
                let mut text = String::new();
                for e in m.exports.iter().filter(|e| e.kind == Kind::Func) {
                    text.push_str(&format!("{} {}\n", e.index, e.name));
                }
                Names::from_text(&text)
            })
    });
    let nimp = m.imported(Kind::Func);
    let mut imported = m.imports.iter().filter(|i| i.ty.kind() == Kind::Func);
    for index in 0..nimp + m.funcs.len() as u32 {
        let ty = match m.func_type(index) {
            Some(t) => format!("{t}"),
            None => String::from("?"),
        };
        let size = match index.checked_sub(nimp) {
            Some(i) => match m.bodies.get(i as usize) {
                Some(n) => format!("{n}"),
                None => String::from("-"),
            },
            None => String::from("import"),
        };
        let import = imported.next().map(|i| format!("{}.{}", i.module, i.name));
        let name = names
            .get(index)
            .map(String::from)
            .or(import)
            .unwrap_or_else(|| String::from("-"));
        println!("func {index} {size} {ty} {name}");
    }
}
