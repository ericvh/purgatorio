//! What the debugging commands share (`docs/debug.md`): the function
//! names of a module, writing a prog's `ctl`, and running a command under
//! one of the three profiles, `prof` (samples), `cprof` (calls) and
//! `mprof` (memory grown), and printing it.

use std::collections::BTreeMap;

use purgatorio::arg::Arg;
use purgatorio::{Error, Waitmsg};
use wasmobj::{Kind, Names};

/// The function names of the module at `path`: its `.names` file, else
/// its own `name` section, else the names of its exported functions.
pub fn module_names(path: &str) -> Names {
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

/// The name of function `func` in `names`, or `func <index>`.
pub fn func_name(names: &Names, func: u32) -> String {
    match names.get(func) {
        Some(n) => String::from(n),
        None => format!("func {func}"),
    }
}

/// Writes `msg` to `/prog/<pid>/ctl`.
pub fn ctl(pid: u32, msg: &str) -> Result<(), Error> {
    std::fs::write(format!("/prog/{pid}/ctl"), msg).map_err(|_| Error::last())
}

/// Reads `/prog/<pid>/<file>`.
pub fn prog_file(pid: u32, file: &str) -> Result<String, Error> {
    std::fs::read_to_string(format!("/prog/{pid}/{file}")).map_err(|_| Error::last())
}

/// What a profile counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Samples at the VM's check points: `prof`.
    Samples,
    /// Calls of each function: `cprof`.
    Calls,
    /// Memory grown in each function and what it called: `mprof`.
    Pages,
}

impl Profile {
    /// The command, and its `ctl` switch and `/prog` file.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Samples => "prof",
            Profile::Calls => "cprof",
            Profile::Pages => "mprof",
        }
    }

    fn usage(self) -> String {
        match self {
            Profile::Calls => String::from("cprof [-u] [-n lines] command [arg ...]"),
            _ => format!("{} [-n lines] command [arg ...]", self.name()),
        }
    }

    /// The word in a module's heading line.
    fn unit(self) -> &'static str {
        match self {
            Profile::Samples => "samples",
            Profile::Calls => "calls",
            Profile::Pages => "bytes",
        }
    }
}

/// A 64 KiB page of wasm memory.
const PAGE: u64 = 65536;

/// The `main` of `prof`, `cprof` and `mprof`: turns the profile on for
/// this prog, runs the command, and prints the profile on standard error,
/// after the command's own output. Exits with the command's status.
pub fn profile(what: Profile, args: Vec<String>) -> Result<(), Error> {
    let usage = what.usage();
    let mut arg = Arg::new(args, &usage);
    let mut lines = 20usize;
    let mut uncalled = false;
    while let Some(c) = arg.opt() {
        match c {
            'n' => {
                lines = arg
                    .earg()?
                    .parse()
                    .map_err(|_| Error(format!("usage: {usage}")))?;
            }
            'u' if what == Profile::Calls => uncalled = true,
            _ => return Err(arg.usage()),
        }
    }
    let argv = arg.argv();
    if argv.is_empty() {
        return Err(Error(format!("usage: {usage}")));
    }
    let path =
        purgatorio::find_command(&argv[0]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
    let me = purgatorio::pid();
    ctl(me, &format!("{} on", what.name()))?;
    let child =
        purgatorio::spawn(&path, &argv, 0, &[]).map_err(|e| Error(format!("{}: {e}", argv[0])))?;
    let status = wait_for(child)?.status;
    // A traced prog that trapped is still there, broken.
    if prog_file(child, "exception").is_ok_and(|e| !e.is_empty()) {
        eprintln!("{}: {child} \"{}\": broken: {status}", what.name(), argv[0]);
        ctl(child, "kill")?;
    }
    let text = prog_file(me, what.name())?;
    report(what, &text, lines, uncalled);
    if !status.is_empty() {
        purgatorio::exits(&status);
    }
    Ok(())
}

/// Waits for child `pid` to exit.
pub fn wait_for(pid: u32) -> Result<Waitmsg, Error> {
    loop {
        let w = purgatorio::wait()?;
        if w.pid == pid {
            return Ok(w);
        }
    }
}

/// The functions a module defines, as `(first index, count)`.
fn defined(path: &str) -> Option<(u32, u32)> {
    let wasm = std::fs::read(path).ok()?;
    let m = wasmobj::parse(&wasm).ok()?;
    Some((m.imported(Kind::Func), m.funcs.len() as u32))
}

/// Prints the profile in `text`, the contents of a `prof`, `cprof` or
/// `mprof` file: per module, most first, a heading and then a line per
/// function, most first.
///
/// - `prof`: `module <path> samples <n>`; each line the share of the
///   module's samples, the samples, the function.
/// - `cprof`: `module <path> calls <n> functions <called> of <defined>`;
///   each line the share of the calls, the calls, the function. With
///   `uncalled`, the functions never called instead, by index.
/// - `mprof`: `module <path> bytes <n>`, the memory the module's
///   outermost function grew, which is all the module grew; each line
///   that function's share, its bytes, the function. A function's bytes
///   include those of the functions it called.
pub fn report(what: Profile, text: &str, lines: usize, uncalled: bool) {
    let mut modules: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    for line in text.lines() {
        let fields = purgatorio::unquote_fields(line);
        let [n, func, path] = fields.as_slice() else {
            continue;
        };
        let (Ok(n), Ok(func)) = (n.parse::<u64>(), func.parse::<u32>()) else {
            continue;
        };
        let n = if what == Profile::Pages { n * PAGE } else { n };
        modules.entry(path.clone()).or_default().push((n, func));
    }
    // `(total, path)` of each module. Most first; equal totals in path
    // order.
    let mut totals: Vec<(u64, String)> = modules
        .iter()
        .map(|(path, funcs)| {
            let total = match what {
                Profile::Pages => funcs.iter().map(|f| f.0).max().unwrap_or(0),
                _ => funcs.iter().map(|f| f.0).sum(),
            };
            (total, path.clone())
        })
        .collect();
    totals.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (total, path) in totals {
        let mut funcs = modules.remove(&path).unwrap_or_default();
        let names = module_names(&path);
        let unit = what.unit();
        match (what, defined(&path)) {
            (Profile::Calls, Some((first, n))) => {
                eprintln!(
                    "module {path} {unit} {total} functions {} of {n}",
                    funcs.len()
                );
                if uncalled {
                    for func in first..first + n {
                        if !funcs.iter().any(|f| f.1 == func) {
                            eprintln!("{}", func_name(&names, func));
                        }
                    }
                    continue;
                }
            }
            _ => eprintln!("module {path} {unit} {total}"),
        }
        funcs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let shown = if lines == 0 { funcs.len() } else { lines };
        for (n, func) in funcs.into_iter().take(shown) {
            let share = n as f64 * 100.0 / total.max(1) as f64;
            eprintln!("{share:5.1}% {n:7} {}", func_name(&names, func));
        }
    }
}
