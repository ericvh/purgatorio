//! fs verb arg...: work on file trees as values. An argument is a word or
//! a `{verb arg...}` block that yields a value for its place, so
//!
//! ```text
//! fs print {filter {match '*.rs'} {walk src}}
//! fs write /n/backup {filter -d {not {match target}} {walk .}}
//! fs bundle {walk doc} > doc.bundle;  fs ls {unbundle doc.bundle}
//! ```
//!
//! Values are a tree (a root and the files under it, with their data), a
//! list of entries (names and stat only), a gate (a test of one entry) or
//! a string. A string where a tree is wanted is walked, a tree where
//! entries are wanted gives its entries, and a string where a gate is
//! wanted is a pattern for `match`. A tree or entries left at the end are
//! printed.
//!
//! Trees: `walk path`; `filter [-d] gate tree` drops what fails the gate,
//! a directory with everything under it (the root always stays), and
//! with `-d` tests files only; `merge tree tree...` is the union under
//! the first root, the earlier tree winning a clash of files; `unbundle
//! file` reads an archive (`-` is standard input); `eval string` parses
//! and runs `string`.
//!
//! Entries: `entries tree`; `select gate entries` keeps what passes.
//!
//! Gates: `match [-a] [-r] pattern` tests the name (or with `-a` the
//! whole path) against a shell pattern (or with `-r` a regular
//! expression); `mode spec` tests the mode as chmod(1) writes it (`+d`,
//! `-d`, `u+w`, `go=r`); `path [-x] path...` passes the paths given, the
//! directories on the way to them and what lies under them, or with `-x`
//! everything but what lies under them; `depth n` passes entries at most
//! `n` levels below the root; `query command` runs `command` under `sh`
//! with `$file` set to the path and passes on success; `and gate...`,
//! `or gate...`, `not gate`.
//!
//! Verbs that yield nothing: `print entries` writes each path; `ls [-um]
//! entries` writes a line as `ls -l` does (`-u` the access time, `-m` the
//! last modifier too); `size entries` writes the total length of the
//! files; `write dir tree` makes the tree under `dir`, with modes and
//! modification times; `bundle tree` writes an archive of it on standard
//! output; `exec [-n n] command entries` runs `command` under `sh` with
//! `$file` set to the paths, `n` at a time (default 1).
//!
//! Follows Inferno's fs(1), with its modules built in. Not here: `pipe`,
//! `proto`, `run`, `setroot`, `chstat`, `compose`, `mergewrite`, `void`,
//! `merge`'s `-1` and `-c`, `exec`'s `-p`, `-P` and `-t`, and `@{...}`
//! command blocks (a command is a quoted string). The bundle format is
//! this command's own (`tree::bundle`).

mod expr;
mod gate;
mod tree;

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::rc::Rc;

use expr::Expr;
use gate::Gate;
use purg_styx::{DMDIR, Dir};
use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::abi::pctl::NEWFD;
use purgatorio::{Context, Error, daytime};
use tree::{Data, Entry, Item, Tree};

enum Value {
    Tree(Tree),
    Entries(Vec<Entry>),
    Gate(Gate),
    Str(String),
    Void,
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Tree(_) => "tree",
            Value::Entries(_) => "entries",
            Value::Gate(_) => "gate",
            Value::Str(_) => "string",
            Value::Void => "nothing",
        }
    }
}

/// The state of one run: whether anything was reported.
struct Fs {
    failed: bool,
}

type Res<T> = Result<T, String>;

fn sys(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

fn read_dir(path: &str) -> Res<Vec<Dir>> {
    let fd = purgatorio::open(path, OREAD).map_err(|e| sys(path, e))?;
    let mut all: Vec<Dir> = Vec::new();
    loop {
        let batch = purgatorio::dirread(fd.as_raw_fd()).map_err(|e| sys(path, e))?;
        if batch.is_empty() {
            break;
        }
        for d in batch {
            if !all.iter().any(|e| e.name == d.name) {
                all.push(d);
            }
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

/// Runs `sh -c line arg...` with this prog's standard files and returns
/// its exit status.
fn sh(line: &str, args: &[String]) -> Res<String> {
    let mut argv = vec![String::from("sh"), String::from("-c"), String::from(line)];
    argv.extend_from_slice(args);
    let pid = purgatorio::spawn("/bin/sh.wasm", &argv, NEWFD, &[(0, 0), (1, 1), (2, 2)])
        .map_err(|e| sys("sh", e))?;
    loop {
        let w = purgatorio::wait().map_err(|e| sys("wait", e))?;
        if w.pid == pid {
            return Ok(w.status);
        }
    }
}

/// The options at the front of `args`, each one of `known`.
fn options<'a>(verb: &str, args: &'a [Value], known: &str) -> Res<(Vec<char>, &'a [Value])> {
    let mut opts = Vec::new();
    let mut rest = args;
    while let Some((Value::Str(s), tail)) = rest.split_first() {
        let Some(letters) = s.strip_prefix('-').filter(|l| !l.is_empty()) else {
            break;
        };
        if !letters.chars().all(|c| known.contains(c)) {
            return Err(format!("{verb}: unknown option {s}"));
        }
        opts.extend(letters.chars());
        rest = tail;
    }
    Ok((opts, rest))
}

impl Fs {
    fn warn(&mut self, msg: &str) {
        eprintln!("fs: {msg}");
        self.failed = true;
    }

    fn walk(&mut self, root: &str) -> Res<Tree> {
        let dir = purgatorio::stat(root).map_err(|e| sys(root, e))?;
        let is_dir = dir.mode & DMDIR != 0;
        let mut items = vec![Item {
            rel: Vec::new(),
            dir,
            data: if is_dir {
                Data::None
            } else {
                Data::File(String::from(root))
            },
        }];
        if is_dir {
            self.walk_into(root, &mut Vec::new(), &mut items);
        }
        Ok(Tree {
            root: String::from(root),
            items,
        })
    }

    fn walk_into(&mut self, root: &str, rel: &mut Vec<String>, items: &mut Vec<Item>) {
        let here = tree::join(root, rel);
        let dirs = match read_dir(&here) {
            Ok(d) => d,
            Err(e) => return self.warn(&e),
        };
        for dir in dirs {
            rel.push(dir.name.clone());
            let is_dir = dir.mode & DMDIR != 0;
            items.push(Item {
                rel: rel.clone(),
                dir,
                data: if is_dir {
                    Data::None
                } else {
                    Data::File(tree::join(root, rel))
                },
            });
            if is_dir {
                self.walk_into(root, rel, items);
            }
            rel.pop();
        }
    }

    fn tree(&mut self, verb: &str, v: Value) -> Res<Tree> {
        match v {
            Value::Tree(t) => Ok(t),
            Value::Str(s) => self.walk(&s),
            v => Err(format!("{verb}: expected a tree, not {}", v.kind())),
        }
    }

    fn entries(&mut self, verb: &str, v: Value) -> Res<Vec<Entry>> {
        match v {
            Value::Entries(e) => Ok(e),
            v => Ok(self.tree(verb, v)?.entries()),
        }
    }

    /// Makes `tree` under `dest`.
    fn write(&mut self, dest: &str, tree: &Tree) {
        // Directories that could not be filled with their own mode get it
        // once everything is in them.
        let mut later = Vec::new();
        for item in &tree.items {
            let target = tree::join(dest, &item.rel);
            let perm = item.dir.mode & 0o777;
            if item.is_dir() {
                match purgatorio::stat(&target) {
                    Ok(d) if d.mode & DMDIR != 0 => {}
                    Ok(_) => self.warn(&format!("{target}: not a directory")),
                    Err(_) => {
                        if let Err(e) = purgatorio::create(&target, OREAD, DMDIR | perm | 0o700) {
                            self.warn(&sys(&target, e));
                        } else if perm & 0o700 != 0o700 {
                            later.push((target, perm));
                        }
                    }
                }
                continue;
            }
            // Made writable, whatever its mode is to be: not every file
            // server lets a file be created read-only and then written.
            let done = item.read().and_then(|data| {
                let fd = purgatorio::create(&target, OWRITE, perm | 0o200).map_err(|e| e.0)?;
                std::fs::File::from(fd)
                    .write_all(&data)
                    .map_err(|e| Error::from(e).0)
            });
            if let Err(e) = done {
                self.warn(&sys(&target, e));
                continue;
            }
            let mut d = Dir::null();
            d.mtime = item.dir.mtime;
            // Not every file server keeps a time it is given.
            let _ = purgatorio::wstat(&target, &d);
            if perm & 0o200 == 0 {
                let mut d = Dir::null();
                d.mode = perm;
                if let Err(e) = purgatorio::wstat(&target, &d) {
                    self.warn(&sys(&target, e));
                }
            }
        }
        for (target, perm) in later.into_iter().rev() {
            let mut d = Dir::null();
            d.mode = DMDIR | perm;
            let _ = purgatorio::wstat(&target, &d);
        }
    }

    fn eval(&mut self, e: &Expr) -> Res<Value> {
        match e {
            Expr::Word(w) => Ok(Value::Str(w.clone())),
            Expr::Block(list) => self.apply(list),
        }
    }

    fn apply(&mut self, list: &[Expr]) -> Res<Value> {
        let Some((Expr::Word(verb), rest)) = list.split_first() else {
            return Err(String::from("an expression starts with a verb"));
        };
        let verb = verb.as_str();
        let mut args = Vec::new();
        for e in rest {
            args.push(self.eval(e)?);
        }
        let usage = |text: &str| Err(format!("usage: {text}"));
        match verb {
            "walk" => match args.as_slice() {
                [Value::Str(path)] => Ok(Value::Tree(self.walk(path)?)),
                _ => usage("walk path"),
            },
            "entries" => match <[Value; 1]>::try_from(args) {
                Ok([v]) => Ok(Value::Entries(self.entries(verb, v)?)),
                Err(_) => usage("entries tree"),
            },
            "eval" => match args.as_slice() {
                [Value::Str(text)] => {
                    let list = expr::parse(text)?;
                    self.apply(&list)
                }
                _ => usage("eval string"),
            },
            "unbundle" => match args.as_slice() {
                [Value::Str(file)] => {
                    let mut all = Vec::new();
                    if file == "-" {
                        std::io::stdin()
                            .read_to_end(&mut all)
                            .map_err(|e| sys("standard input", Error::from(e)))?;
                    } else {
                        all = std::fs::read(file).map_err(|e| sys(file, Error::from(e)))?;
                    }
                    let t = tree::unbundle(Rc::new(all)).map_err(|e| sys(file, e))?;
                    Ok(Value::Tree(t))
                }
                _ => usage("unbundle file"),
            },
            "filter" => {
                let (opts, rest) = options(verb, &args, "d")?;
                let n = args.len() - rest.len();
                match <[Value; 2]>::try_from(args.split_off(n)) {
                    Ok([g, t]) => {
                        let g = gate_of(verb, g)?;
                        let t = self.tree(verb, t)?;
                        Ok(Value::Tree(t.filter(&g, !opts.contains(&'d'))))
                    }
                    _ => usage("filter [-d] gate tree"),
                }
            }
            "select" => match <[Value; 2]>::try_from(args) {
                Ok([g, e]) => {
                    let g = gate_of(verb, g)?;
                    let mut e = self.entries(verb, e)?;
                    e.retain(|e| g(e));
                    Ok(Value::Entries(e))
                }
                _ => usage("select gate entries"),
            },
            "merge" => {
                if args.is_empty() {
                    return usage("merge tree tree...");
                }
                let mut trees = Vec::new();
                for a in args {
                    trees.push(self.tree(verb, a)?);
                }
                Ok(Value::Tree(tree::merge(trees)))
            }
            "match" => {
                let (opts, rest) = options(verb, &args, "ar")?;
                let [Value::Str(pat)] = rest else {
                    return usage("match [-a] [-r] pattern");
                };
                Ok(Value::Gate(match_gate(
                    pat,
                    opts.contains(&'a'),
                    opts.contains(&'r'),
                )?))
            }
            "mode" => match args.as_slice() {
                [Value::Str(spec)] => {
                    let clauses = gate::parse_mode(spec)?;
                    Ok(Value::Gate(Box::new(move |e| {
                        gate::mode_ok(&clauses, e.dir.mode)
                    })))
                }
                _ => usage("mode spec"),
            },
            "path" => {
                let (opts, rest) = options(verb, &args, "x")?;
                let mut paths = Vec::new();
                for a in rest {
                    match a {
                        Value::Str(s) => paths.push(s.clone()),
                        _ => return usage("path [-x] path..."),
                    }
                }
                if paths.is_empty() {
                    return usage("path [-x] path...");
                }
                let exact = opts.contains(&'x');
                Ok(Value::Gate(Box::new(move |e| {
                    if exact {
                        !paths.iter().any(|p| gate::under(&e.path, p))
                    } else {
                        paths.iter().any(|p| gate::path_ok(&e.path, p))
                    }
                })))
            }
            "depth" => match args.as_slice() {
                [Value::Str(n)] => {
                    let n: usize = n.parse().map_err(|_| format!("depth: bad number {n}"))?;
                    Ok(Value::Gate(Box::new(move |e| e.depth <= n)))
                }
                _ => usage("depth n"),
            },
            "query" => match args.as_slice() {
                [Value::Str(cmd)] => {
                    let cmd = cmd.clone();
                    Ok(Value::Gate(Box::new(move |e| {
                        let line = format!("file=$*; {cmd}");
                        sh(&line, std::slice::from_ref(&e.path)).is_ok_and(|s| s.is_empty())
                    })))
                }
                _ => usage("query command"),
            },
            "and" | "or" => {
                let mut gates = Vec::new();
                for a in args {
                    gates.push(gate_of(verb, a)?);
                }
                if gates.is_empty() {
                    return Err(format!("usage: {verb} gate..."));
                }
                let all = verb == "and";
                Ok(Value::Gate(Box::new(move |e| {
                    if all {
                        gates.iter().all(|g| g(e))
                    } else {
                        gates.iter().any(|g| g(e))
                    }
                })))
            }
            "not" => match <[Value; 1]>::try_from(args) {
                Ok([g]) => {
                    let g = gate_of(verb, g)?;
                    Ok(Value::Gate(Box::new(move |e| !g(e))))
                }
                Err(_) => usage("not gate"),
            },
            "print" => match <[Value; 1]>::try_from(args) {
                Ok([v]) => {
                    let e = self.entries(verb, v)?;
                    print(&e)?;
                    Ok(Value::Void)
                }
                Err(_) => usage("print entries"),
            },
            "ls" => {
                let (opts, rest) = options(verb, &args, "um")?;
                let n = args.len() - rest.len();
                let Ok([v]) = <[Value; 1]>::try_from(args.split_off(n)) else {
                    return usage("ls [-um] entries");
                };
                let now = daytime::now();
                let mut out = String::new();
                for e in self.entries(verb, v)? {
                    let d = &e.dir;
                    let t = if opts.contains(&'u') {
                        d.atime
                    } else {
                        d.mtime
                    };
                    out.push_str(&format!("{} {} {} ", mode_string(d.mode), d.uid, d.gid));
                    if opts.contains(&'m') {
                        out.push_str(&format!("[{}] ", d.muid));
                    }
                    out.push_str(&format!(
                        "{} {} {}\n",
                        d.length,
                        daytime::filet(now, i64::from(t)),
                        e.path
                    ));
                }
                put(out.as_bytes())?;
                Ok(Value::Void)
            }
            "size" => match <[Value; 1]>::try_from(args) {
                Ok([v]) => {
                    let total: u64 = self
                        .entries(verb, v)?
                        .iter()
                        .filter(|e| e.dir.mode & DMDIR == 0)
                        .map(|e| e.dir.length)
                        .sum();
                    put(format!("{total}\n").as_bytes())?;
                    Ok(Value::Void)
                }
                Err(_) => usage("size entries"),
            },
            "write" => match <[Value; 2]>::try_from(args) {
                Ok([Value::Str(dest), t]) => {
                    let t = self.tree(verb, t)?;
                    self.write(&dest, &t);
                    Ok(Value::Void)
                }
                _ => usage("write dir tree"),
            },
            "bundle" => match <[Value; 1]>::try_from(args) {
                Ok([t]) => {
                    let t = self.tree(verb, t)?;
                    let mut out = Vec::new();
                    let mut warned = Vec::new();
                    tree::bundle(&t, &mut out, &mut |item, e| {
                        warned.push(sys(&tree::join(&t.root, &item.rel), e));
                    })
                    .map_err(|e| sys("bundle", e))?;
                    for w in warned {
                        self.warn(&w);
                    }
                    put(&out)?;
                    Ok(Value::Void)
                }
                Err(_) => usage("bundle tree"),
            },
            "exec" => {
                let text = "exec [-n n] command entries";
                let mut n = 1usize;
                if let Some(Value::Str(s)) = args.first()
                    && s == "-n"
                {
                    let Some(Value::Str(count)) = args.get(1) else {
                        return usage(text);
                    };
                    n = count
                        .parse()
                        .ok()
                        .filter(|&n| n > 0)
                        .ok_or_else(|| format!("usage: {text}"))?;
                    args.drain(..2);
                }
                let Ok([Value::Str(cmd), v]) = <[Value; 2]>::try_from(args) else {
                    return usage(text);
                };
                let paths: Vec<String> =
                    self.entries(verb, v)?.into_iter().map(|e| e.path).collect();
                // The paths are `sh -c`'s arguments, and become `$file`.
                let line = format!("file=$*; {cmd}");
                for group in paths.chunks(n) {
                    let status = sh(&line, group)?;
                    if !status.is_empty() {
                        self.warn(&format!("{cmd}: {status}"));
                    }
                }
                Ok(Value::Void)
            }
            _ => Err(format!("{verb}: unknown verb")),
        }
    }
}

/// The gate `match [-a] [-r] pat`.
fn match_gate(pat: &str, whole: bool, regex: bool) -> Res<Gate> {
    let pick = move |e: &Entry| {
        if whole {
            e.path.clone()
        } else {
            e.name.clone()
        }
    };
    if regex {
        let re = regexp::Regex::new(pat).map_err(|e| sys(pat, e))?;
        return Ok(Box::new(move |e| re.is_match(&pick(e))));
    }
    let pat = String::from(pat);
    Ok(Box::new(move |e| {
        // A pattern is matched an element at a time; a whole path
        // matches when its elements all do.
        let (p, n) = (pat.split('/'), pick(e));
        p.clone().count() == n.split('/').count()
            && p.zip(n.split('/')).all(|(p, n)| filepat::matches(p, n))
    }))
}

/// A gate argument; a string is a pattern for `match`.
fn gate_of(verb: &str, v: Value) -> Res<Gate> {
    match v {
        Value::Gate(g) => Ok(g),
        Value::Str(pat) => match_gate(&pat, false, false),
        v => Err(format!("{verb}: expected a gate, not {}", v.kind())),
    }
}

fn put(bytes: &[u8]) -> Res<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)
        .and_then(|()| out.flush())
        .map_err(|e| sys("write", Error::from(e)))
}

fn print(entries: &[Entry]) -> Res<()> {
    let mut out = String::new();
    for e in entries {
        out.push_str(&e.path);
        out.push('\n');
    }
    put(out.as_bytes())
}

fn mode_string(m: u32) -> String {
    let mut s = String::new();
    s.push(if m & DMDIR != 0 {
        'd'
    } else if m & purg_styx::DMAPPEND != 0 {
        'a'
    } else {
        '-'
    });
    s.push(if m & purg_styx::DMEXCL != 0 { 'l' } else { '-' });
    for shift in [6, 3, 0] {
        let b = m >> shift;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        return Err(Error::from("usage: fs verb arg..."));
    }
    let list = expr::from_args(&args[1..]).map_err(Error)?;
    let mut fs = Fs { failed: false };
    // A tree or entries left at the end are printed.
    // `fs {verb arg...}` is `fs verb arg...`.
    let list = match list.as_slice() {
        [Expr::Block(inner)] => inner.clone(),
        _ => list,
    };
    match fs.apply(&list).map_err(Error)? {
        Value::Void => {}
        Value::Tree(t) => print(&t.entries()).map_err(Error)?,
        Value::Entries(e) => print(&e).map_err(Error)?,
        v => {
            return Err(Error(format!(
                "the expression yields a {}, which is not printed",
                v.kind()
            )));
        }
    }
    if fs.failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
