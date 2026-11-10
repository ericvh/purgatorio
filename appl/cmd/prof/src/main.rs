//! prof [-n lines] command [arg ...]: run a command and print where it
//! spent its time, by function, as Inferno's `prof` does by line of
//! Limbo.
//!
//! `prof` turns profiling on for itself (`prof on` to its `ctl`,
//! `docs/debug.md`), so the command and every prog it starts are traced
//! and sampled, and their samples come back to `prof` as they exit. A
//! sample names the function a prog was in at one of the VM's check
//! points: every 10 ms of running under wasmtime, every 2^20 units of
//! fuel (about as many instructions) under wasmi. Time a prog spends
//! blocked in the system is not sampled.
//!
//! For each module that was sampled, most samples first, it prints
//! `module <path> samples <n>` and then a line per function, most samples
//! first: the share of the module's samples, the samples, and the
//! function's name (from the module's `.names` file, as `stack` finds
//! it). `-n` limits the lines per module (default 20; 0 for all).
//! The output goes to standard error, after the command's own. The exit
//! status is the command's.
//!
//! A profiled prog that traps is left broken, as any traced prog is;
//! `prof` reports it and kills it.

use std::collections::BTreeMap;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use wasmobj::{Kind, Names};

const USAGE: &str = "prof [-n lines] command [arg ...]";

/// Finds `name` as the shell would: a path as given or with `.wasm`, or
/// a bare name in /bin.
fn resolve(name: &str) -> Result<String, Error> {
    let candidates = if name.starts_with('/') || name.starts_with("./") || name.starts_with("../") {
        vec![String::from(name), format!("{name}.wasm")]
    } else {
        vec![format!("/bin/{name}.wasm")]
    };
    let mut err = String::from(purgatorio::abi::error::ENONEXIST);
    for c in &candidates {
        match purgatorio::stat(c) {
            Ok(d) if !d.qid.is_dir() => return Ok(c.clone()),
            Ok(_) => err = String::from(purgatorio::abi::error::EISDIR),
            Err(e) => err = e.0,
        }
    }
    Err(Error(err))
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

fn ctl(pid: u32, msg: &str) -> Result<(), Error> {
    std::fs::write(format!("/prog/{pid}/ctl"), msg).map_err(|_| Error::last())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut lines = 20usize;
    while let Some(c) = arg.opt() {
        match c {
            'n' => {
                lines = arg
                    .earg()?
                    .parse()
                    .map_err(|_| Error(format!("usage: {USAGE}")))?;
            }
            _ => return Err(arg.usage()),
        }
    }
    let argv = arg.argv();
    if argv.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let path = resolve(&argv[0]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
    let me = purgatorio::pid();
    ctl(me, "prof on")?;
    let child =
        purgatorio::spawn(&path, &argv, 0, &[]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
    let status = loop {
        let w = purgatorio::wait()?;
        if w.pid == child {
            break w.status;
        }
    };
    // A traced prog that trapped is still there, broken.
    if std::fs::read_to_string(format!("/prog/{child}/exception")).is_ok_and(|e| !e.is_empty()) {
        eprintln!("prof: {child} \"{}\": broken: {status}", argv[0]);
        ctl(child, "kill")?;
    }
    let text = std::fs::read_to_string(format!("/prog/{me}/prof")).map_err(|_| Error::last())?;
    report(&text, lines);
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}

/// Prints the profile in `text`, the contents of a `prof` file.
fn report(text: &str, lines: usize) {
    let mut modules: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    for line in text.lines() {
        let fields = purgatorio::unquote_fields(line);
        let [n, func, path] = fields.as_slice() else {
            continue;
        };
        let (Ok(n), Ok(func)) = (n.parse::<u64>(), func.parse::<u32>()) else {
            continue;
        };
        modules.entry(path.clone()).or_default().push((n, func));
    }
    // `(samples, path)` of each module. Most samples first; equal counts
    // in path, or index, order.
    let mut totals: Vec<(u64, String)> = modules
        .iter()
        .map(|(path, funcs)| (funcs.iter().map(|f| f.0).sum(), path.clone()))
        .collect();
    totals.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (total, path) in totals {
        let mut funcs = modules.remove(&path).unwrap_or_default();
        eprintln!("module {path} samples {total}");
        let names = module_names(&path);
        funcs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let shown = if lines == 0 { funcs.len() } else { lines };
        for (n, func) in funcs.into_iter().take(shown) {
            let name = match names.get(func) {
                Some(n) => String::from(n),
                None => format!("func {func}"),
            };
            let share = n as f64 * 100.0 / total.max(1) as f64;
            eprintln!("{share:5.1}% {n:7} {name}");
        }
    }
}
