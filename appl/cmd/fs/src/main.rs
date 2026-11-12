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
//! A command is an `@{...}` block or a quoted string, run by `sh`.
//! `exec [-pP] [-t command] [-n n] command entries` runs the command for
//! `n` entries at a time (default 1) with `$file` set to their paths, all
//! in one shell, so variables set by one run are seen by the next; `-p`
//! also sets `mode`, `uid`, `mtime` and `length` from the entry, `-P`
//! those and `name`, `gid`, `muid`, `qid`, `atime`, `dtype` and `dev`
//! (both need `-n 1`); `-t command` runs once at the end with `$file`
//! empty. `pipe [-1pP] command tree` runs the command once per file with
//! the file's data as its standard input and `$file` set, or with `-1`
//! once for the whole tree, every file's data in turn. `run command`
//! yields the value of `$s` the command leaves, which must be one word;
//! a command where a string is wanted is run.
//!
//! More trees: `setroot [-c] path tree` names the root `path`, or with
//! `-c` makes each element of `path` a directory of its own; `proto [-r
//! root] protofile` is the tree a proto(6) file selects from `root`
//! (default `/`); `chstat [-c] [-p gate] [-m mode] [-u uid] [-g gid] [-t
//! mtime] [-a atime] tree` changes the attributes of the entries the gate
//! passes (all, without `-p`): `-m` as chmod(1) does, `-c` first copies
//! the owner's permissions to group and others. `merge [-1] [-c selector]
//! tree...` takes `selector` to choose between entries of the same name
//! (default `{compose AoverB}`); `-1` merges only the roots, taking each
//! directory under them from one side. `compose [-d] op` is the selector
//! of a Porter-Duff operator: `AinB BinA AoutB BoutA A B AoverB BoverA
//! AatopB BatopA AxorB`; with `-d` two directories an operator would drop
//! are kept so that what is in them is compared.
//!
//! `mergewrite [-c selector] dir tree` writes the tree into `dir` over
//! what is there: the selector sees the new entry as A and the one in
//! `dir` as B, A is written when it chooses A, and B is removed when it
//! does not choose B (by default both are chosen: files are rewritten,
//! nothing is removed). `write` with an empty `dir` writes to the tree's
//! own root. `void value` discards a value.
//!
//! Follows Inferno's fs(1), with its modules built in. Where it differs:
//! `mergewrite` takes its selector as `-c`. The bundle format is this
//! command's own (`tree::bundle`).

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
use tree::{Data, Entry, Item, Node, Selector, Tree};

enum Value {
    Tree(Tree),
    Entries(Vec<Entry>),
    Gate(Gate),
    Selector(Selector),
    Str(String),
    /// The text of an `@{...}` block.
    Cmd(String),
    Void,
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Tree(_) => "tree",
            Value::Entries(_) => "entries",
            Value::Gate(_) => "gate",
            Value::Selector(_) => "selector",
            Value::Str(_) => "string",
            Value::Cmd(_) => "command",
            Value::Void => "nothing",
        }
    }
}

/// Options as [`getopts`] gives them: each letter and its value.
type Opts = Vec<(char, Option<Value>)>;

/// The options at the front of `args`: the letters in `flags` stand
/// alone, those in `with` take the next value (or the rest of the word).
fn getopts(verb: &str, mut args: Vec<Value>, flags: &str, with: &str) -> Res<(Opts, Vec<Value>)> {
    let mut opts = Vec::new();
    let mut rest = std::collections::VecDeque::from(std::mem::take(&mut args));
    while let Some(Value::Str(s)) = rest.front() {
        let Some(letters) = s.strip_prefix('-').filter(|l| !l.is_empty()) else {
            break;
        };
        let letters = String::from(letters);
        rest.pop_front();
        for (i, c) in letters.char_indices() {
            if flags.contains(c) {
                opts.push((c, None));
            } else if with.contains(c) {
                let tail = &letters[i + c.len_utf8()..];
                let v = if tail.is_empty() {
                    rest.pop_front()
                        .ok_or_else(|| format!("{verb}: option -{c} needs a value"))?
                } else {
                    Value::Str(String::from(tail))
                };
                opts.push((c, Some(v)));
                break;
            } else {
                return Err(format!("{verb}: unknown option -{c}"));
            }
        }
    }
    Ok((opts, rest.into()))
}

/// `s` quoted for sh.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Assignments of the stat variables `exec -p` and `-P` set.
fn stat_vars(d: &Dir, all: bool) -> String {
    let mut v = format!(
        "mode={}; uid={}; mtime={}; length={}; ",
        quote(&mode_string(d.mode)),
        quote(&d.uid),
        d.mtime,
        d.length
    );
    if all {
        let dtype = char::from_u32(u32::from(d.dtype)).unwrap_or('?');
        v.push_str(&format!(
            "name={}; gid={}; muid={}; qid=(16r{:x} {}); atime={}; dtype={}; dev={}; ",
            quote(&d.name),
            quote(&d.gid),
            quote(&d.muid),
            d.qid.path,
            d.qid.vers,
            d.atime,
            quote(&dtype.to_string()),
            d.dev
        ));
    }
    v
}

/// Runs `sh -c line` with a pipe as its standard input, gives `feed` the
/// other end to write, and waits for it.
fn sh_feed(line: &str, feed: &mut dyn FnMut(&mut std::fs::File)) -> Res<String> {
    let [r, w] = purgatorio::pipe().map_err(|e| sys("pipe", e))?;
    let argv = [String::from("sh"), String::from("-c"), String::from(line)];
    let pid = purgatorio::spawn(
        "/bin/sh.wasm",
        &argv,
        NEWFD,
        &[(r.as_raw_fd(), 0), (1, 1), (2, 2)],
    )
    .map_err(|e| sys("sh", e))?;
    drop(r);
    feed(&mut std::fs::File::from(w));
    wait_for(pid)
}

/// Runs `command` and returns the one word it leaves in `$s`.
fn run(cmd: &str) -> Res<String> {
    let [r, w] = purgatorio::pipe().map_err(|e| sys("pipe", e))?;
    let line = format!("{cmd}; echo $#s $s >/fd/5");
    let argv = [String::from("sh"), String::from("-c"), line];
    let pid = purgatorio::spawn(
        "/bin/sh.wasm",
        &argv,
        NEWFD,
        &[(0, 0), (1, 1), (2, 2), (w.as_raw_fd(), 5)],
    )
    .map_err(|e| sys("sh", e))?;
    drop(w);
    let mut out = String::new();
    let got = std::fs::File::from(r).read_to_string(&mut out);
    wait_for(pid)?;
    got.map_err(|e| sys("run", Error::from(e)))?;
    let out = out.strip_suffix('\n').unwrap_or(&out);
    match out.split_once(' ') {
        Some(("1", word)) => Ok(String::from(word)),
        _ if out == "1" => Ok(String::new()),
        _ => Err(String::from("run: $s must hold one word")),
    }
}

fn wait_for(pid: u32) -> Res<String> {
    loop {
        let w = purgatorio::wait().map_err(|e| sys("wait", e))?;
        if w.pid == pid {
            return Ok(w.status);
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
    wait_for(pid)
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
            Value::Cmd(c) => self.walk(&run(&c)?),
            v => Err(format!("{verb}: expected a tree, not {}", v.kind())),
        }
    }

    fn string(&mut self, verb: &str, v: Value) -> Res<String> {
        match v {
            Value::Str(s) => Ok(s),
            Value::Cmd(c) => run(&c),
            v => Err(format!("{verb}: expected a string, not {}", v.kind())),
        }
    }

    /// Writes the file `item` as `target`, with its mode and time.
    fn write_file(&mut self, item: &Item, target: &str) {
        let perm = item.dir.mode & 0o777;
        // Made writable, whatever its mode is to be: not every file
        // server lets a file be created read-only and then written.
        let done = item.read().and_then(|data| {
            let fd = purgatorio::create(target, OWRITE, perm | 0o200).map_err(|e| e.0)?;
            std::fs::File::from(fd)
                .write_all(&data)
                .map_err(|e| Error::from(e).0)
        });
        if let Err(e) = done {
            return self.warn(&sys(target, e));
        }
        let mut d = Dir::null();
        d.mtime = item.dir.mtime;
        // Not every file server keeps a time it is given.
        let _ = purgatorio::wstat(target, &d);
        if perm & 0o200 == 0 {
            let mut d = Dir::null();
            d.mode = perm;
            if let Err(e) = purgatorio::wstat(target, &d) {
                self.warn(&sys(target, e));
            }
        }
    }

    /// Removes `path` and, for a directory, what is in it.
    fn remove_all(&mut self, path: &str, d: &Dir) {
        if d.mode & DMDIR != 0 {
            match read_dir(path) {
                Ok(kids) => {
                    for k in kids {
                        self.remove_all(&format!("{path}/{}", k.name), &k);
                    }
                }
                Err(e) => self.warn(&e),
            }
        }
        if let Err(e) = purgatorio::remove(path) {
            self.warn(&sys(path, e));
        }
    }

    /// Makes `target` a directory for `item` if it is not one already,
    /// and writable by its owner until [`Fs::seal`] is given the result.
    fn open_dir(&mut self, item: &Item, target: &str) -> Option<(String, u32)> {
        let perm = item.dir.mode & 0o777;
        match purgatorio::stat(target) {
            Ok(d) if d.mode & DMDIR != 0 => {
                if d.mode & 0o700 == 0o700 {
                    return None;
                }
                let mut w = Dir::null();
                w.mode = d.mode | 0o700;
                let _ = purgatorio::wstat(target, &w);
                Some((String::from(target), d.mode))
            }
            Ok(_) => {
                self.warn(&format!("{target}: not a directory"));
                None
            }
            Err(_) => match purgatorio::create(target, OREAD, DMDIR | perm | 0o700) {
                Err(e) => {
                    self.warn(&sys(target, e));
                    None
                }
                Ok(_) if perm & 0o700 != 0o700 => Some((String::from(target), DMDIR | perm)),
                Ok(_) => None,
            },
        }
    }

    fn seal(&mut self, pending: Option<(String, u32)>) {
        if let Some((target, mode)) = pending {
            let mut d = Dir::null();
            d.mode = mode;
            let _ = purgatorio::wstat(&target, &d);
        }
    }

    /// Writes the directory `node` into the directory `target` over what
    /// is there, as `sel` chooses (A is `node`'s entry, B the one there).
    fn merge_into(&mut self, sel: &Selector, node: &Node, target: &str) {
        let there = match read_dir(target) {
            Ok(d) => d,
            Err(e) => return self.warn(&e),
        };
        let mut names: std::collections::BTreeSet<&str> =
            node.kids.keys().map(String::as_str).collect();
        names.extend(there.iter().map(|d| d.name.as_str()));
        for name in names {
            let path = format!("{}/{name}", target.trim_end_matches('/'));
            let src = node.kids.get(name);
            let mut dst = there.iter().find(|d| d.name == name).map(|d| Item {
                rel: Vec::new(),
                dir: d.clone(),
                data: Data::File(path.clone()),
            });
            let r = tree::choose(sel, src.map(|n| &n.item), dst.as_ref());
            if let Some(d) = &dst
                && r & 2 == 0
            {
                self.remove_all(&path, &d.dir);
                dst = None;
            }
            let Some(src) = src.filter(|_| r & 1 != 0) else {
                continue;
            };
            if src.item.is_dir() {
                let pending = self.open_dir(&src.item, &path);
                if purgatorio::stat(&path).is_ok_and(|d| d.mode & DMDIR != 0) {
                    self.merge_into(sel, src, &path);
                }
                self.seal(pending);
            } else if dst.as_ref().is_some_and(Item::is_dir) {
                self.warn(&format!("{path}: is a directory"));
            } else {
                self.write_file(&src.item, &path);
            }
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
            self.write_file(item, &target);
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
            Expr::Cmd(text) => Ok(Value::Cmd(text.clone())),
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
            "walk" => match <[Value; 1]>::try_from(args) {
                Ok([v @ (Value::Str(_) | Value::Cmd(_))]) => {
                    let path = self.string(verb, v)?;
                    Ok(Value::Tree(self.walk(&path)?))
                }
                _ => usage("walk path"),
            },
            "run" => match <[Value; 1]>::try_from(args) {
                Ok([v]) => Ok(Value::Str(run(&command(verb, v)?)?)),
                Err(_) => usage("run command"),
            },
            "void" => match args.len() {
                1 => Ok(Value::Void),
                _ => usage("void value"),
            },
            "setroot" => {
                let (opts, rest) = getopts(verb, args, "c", "")?;
                let explicit = !opts.is_empty();
                match <[Value; 2]>::try_from(rest) {
                    Ok([p, t]) => {
                        let path = self.string(verb, p)?;
                        let t = self.tree(verb, t)?;
                        if path.is_empty() && !explicit {
                            return Err(String::from("setroot: empty path"));
                        }
                        let path = if path.is_empty() {
                            t.root.clone()
                        } else {
                            path
                        };
                        Ok(Value::Tree(tree::setroot(t, &path, explicit)))
                    }
                    Err(_) => usage("setroot [-c] path tree"),
                }
            }
            "proto" => {
                let (opts, rest) = getopts(verb, args, "", "r")?;
                let mut root = String::from("/");
                for (_, v) in opts {
                    root = self.string(verb, v.unwrap_or(Value::Void))?;
                }
                match <[Value; 1]>::try_from(rest) {
                    Ok([f]) => {
                        let f = self.string(verb, f)?;
                        Ok(Value::Tree(self.proto(&f, &root)?))
                    }
                    Err(_) => usage("proto [-r root] protofile"),
                }
            }
            "chstat" => {
                let (opts, rest) = getopts(verb, args, "c", "pmugta")?;
                let Ok([t]) = <[Value; 1]>::try_from(rest) else {
                    return usage(
                        "chstat [-c] [-p gate] [-m mode] [-u uid] [-g gid] [-t mtime] [-a atime] tree",
                    );
                };
                let mut t = self.tree(verb, t)?;
                let mut gate = None;
                let mut change = Change::default();
                for (c, v) in opts {
                    let v = v.unwrap_or(Value::Void);
                    if c == 'p' {
                        gate = Some(gate_of(verb, v)?);
                        continue;
                    }
                    if c == 'c' {
                        change.copy = true;
                        continue;
                    }
                    let s = self.string(verb, v)?;
                    let num = |s: &str| {
                        s.parse::<u32>()
                            .map_err(|_| format!("chstat: bad time {s}"))
                    };
                    match c {
                        'm' => change.mode = Some(gate::parse_mode(&s)?),
                        'u' => change.uid = Some(s),
                        'g' => change.gid = Some(s),
                        't' => change.mtime = Some(num(&s)?),
                        _ => change.atime = Some(num(&s)?),
                    }
                }
                for i in 0..t.items.len() {
                    if gate.as_ref().is_some_and(|g| !g(&t.entry(&t.items[i]))) {
                        continue;
                    }
                    change.apply(&mut t.items[i].dir);
                }
                Ok(Value::Tree(t))
            }
            "compose" => {
                let (opts, rest) = getopts(verb, args, "d", "")?;
                match <[Value; 1]>::try_from(rest) {
                    Ok([op]) => {
                        let op = self.string(verb, op)?;
                        let sel = tree::compose(&op, !opts.is_empty())
                            .ok_or_else(|| format!("compose: bad op {op}"))?;
                        Ok(Value::Selector(sel))
                    }
                    Err(_) => usage("compose [-d] op"),
                }
            }
            "mergewrite" => {
                let (opts, rest) = getopts(verb, args, "", "c")?;
                let mut sel: Selector = Box::new(|_, _| 3);
                for (_, v) in opts {
                    sel = selector_of(verb, v.unwrap_or(Value::Void))?;
                }
                let Ok([d, t]) = <[Value; 2]>::try_from(rest) else {
                    return usage("mergewrite [-c selector] dir tree");
                };
                let dest = self.string(verb, d)?;
                let t = self.tree(verb, t)?;
                let dest = if dest.is_empty() {
                    t.root.clone()
                } else {
                    dest
                };
                let root = t.nodes();
                if root.item.is_dir() {
                    let pending = self.open_dir(&root.item, &dest);
                    if purgatorio::stat(&dest).is_ok_and(|d| d.mode & DMDIR != 0) {
                        self.merge_into(&sel, &root, &dest);
                    }
                    self.seal(pending);
                } else {
                    self.write_file(&root.item, &dest);
                }
                Ok(Value::Void)
            }
            "pipe" => {
                let (opts, rest) = getopts(verb, args, "1pP", "")?;
                let has = |c: char| opts.iter().any(|(o, _)| *o == c);
                let (once, all) = (has('1'), has('P'));
                let vars = all || has('p');
                if once && vars {
                    return Err(String::from("pipe: -1 cannot be used with -p or -P"));
                }
                let Ok([c, t]) = <[Value; 2]>::try_from(rest) else {
                    return usage("pipe [-1pP] command tree");
                };
                let cmd = command(verb, c)?;
                let t = self.tree(verb, t)?;
                let files: Vec<&Item> = t.items.iter().filter(|i| !i.is_dir()).collect();
                let mut warned = Vec::new();
                let mut send = |f: &mut std::fs::File, item: &Item| match item.read() {
                    // A command that stops reading loses the rest.
                    Ok(data) => {
                        let _ = f.write_all(&data);
                    }
                    Err(e) => warned.push(sys(&tree::join(&t.root, &item.rel), e)),
                };
                if once {
                    sh_feed(&cmd, &mut |f| {
                        for item in &files {
                            send(f, item);
                        }
                    })?;
                } else {
                    for item in &files {
                        let mut line = format!("file={}; ", quote(&tree::join(&t.root, &item.rel)));
                        if vars {
                            line.push_str(&stat_vars(&item.dir, all));
                        }
                        line.push_str(&cmd);
                        sh_feed(&line, &mut |f| send(f, item))?;
                    }
                }
                for w in warned {
                    self.warn(&w);
                }
                Ok(Value::Void)
            }
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
                let (opts, rest) = getopts(verb, args, "1", "c")?;
                if rest.is_empty() {
                    return usage("merge [-1] [-c selector] tree tree...");
                }
                let mut sel = tree::compose("AoverB", false).expect("AoverB");
                let mut deep = true;
                for (c, v) in opts {
                    match v {
                        Some(v) => sel = selector_of(verb, v)?,
                        None if c == '1' => deep = false,
                        None => {}
                    }
                }
                let mut trees = Vec::new();
                for a in rest {
                    trees.push(self.tree(verb, a)?);
                }
                Ok(Value::Tree(tree::merge(trees, &sel, deep)))
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
            "query" => match <[Value; 1]>::try_from(args) {
                Ok([c]) => {
                    let cmd = command(verb, c)?;
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
                Ok([d @ (Value::Str(_) | Value::Cmd(_)), t]) => {
                    let dest = self.string(verb, d)?;
                    let t = self.tree(verb, t)?;
                    let dest = if dest.is_empty() {
                        t.root.clone()
                    } else {
                        dest
                    };
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
                let text = "exec [-pP] [-t command] [-n n] command entries";
                let (opts, rest) = getopts(verb, args, "pP", "nt")?;
                let mut n = 1usize;
                let (mut vars, mut all, mut last) = (false, false, None);
                for (c, v) in opts {
                    match (c, v) {
                        ('p', _) => vars = true,
                        ('P', _) => (vars, all) = (true, true),
                        ('n', Some(v)) => {
                            n = self
                                .string(verb, v)?
                                .parse()
                                .ok()
                                .filter(|&n| n > 0)
                                .ok_or_else(|| format!("usage: {text}"))?;
                        }
                        (_, Some(v)) => last = Some(command(verb, v)?),
                        _ => {}
                    }
                }
                if vars && n > 1 {
                    return Err(String::from("exec: -p and -P need -n 1"));
                }
                let Ok([c, v]) = <[Value; 2]>::try_from(rest) else {
                    return usage(text);
                };
                let cmd = command(verb, c)?;
                let entries = self.entries(verb, v)?;
                // One shell runs every group, so what one run sets the
                // next sees.
                let mut script = String::new();
                for group in entries.chunks(n) {
                    let names: Vec<String> = group.iter().map(|e| quote(&e.path)).collect();
                    script.push_str(&format!("file=({}); ", names.join(" ")));
                    if vars {
                        script.push_str(&stat_vars(&group[0].dir, all));
                    }
                    script.push_str(&cmd);
                    script.push('\n');
                }
                if let Some(last) = last {
                    script.push_str(&format!("file=(); {last}\n"));
                }
                if !script.is_empty() {
                    sh(&script, &[])?;
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

/// The text sh runs for a command argument: an `@{...}` block, or a
/// string made one.
fn command(verb: &str, v: Value) -> Res<String> {
    match v {
        Value::Cmd(c) => Ok(c),
        Value::Str(s) => Ok(format!("{{{s}}}")),
        v => Err(format!("{verb}: expected a command, not {}", v.kind())),
    }
}

fn selector_of(verb: &str, v: Value) -> Res<Selector> {
    match v {
        Value::Selector(s) => Ok(s),
        v => Err(format!("{verb}: expected a selector, not {}", v.kind())),
    }
}

/// What `chstat` changes.
#[derive(Default)]
struct Change {
    copy: bool,
    mode: Option<Vec<gate::Clause>>,
    uid: Option<String>,
    gid: Option<String>,
    mtime: Option<u32>,
    atime: Option<u32>,
}

impl Change {
    fn apply(&self, d: &mut Dir) {
        if self.copy {
            let u = d.mode & 0o700;
            d.mode = (d.mode & !0o077) | (u >> 3) | (u >> 6);
        }
        if let Some(m) = &self.mode {
            d.mode = gate::apply_mode(m, d.mode);
        }
        if let Some(u) = &self.uid {
            d.uid = u.clone();
        }
        if let Some(g) = &self.gid {
            d.gid = g.clone();
        }
        if let Some(t) = self.mtime {
            d.mtime = t;
        }
        if let Some(t) = self.atime {
            d.atime = t;
        }
    }
}

impl Fs {
    /// The tree the proto(6) file `file` selects from `root`.
    fn proto(&mut self, file: &str, root: &str) -> Res<Tree> {
        let text = std::fs::read_to_string(file).map_err(|e| sys(file, Error::from(e)))?;
        let mut top = purgatorio::stat(root).map_err(|e| sys(root, e))?;
        top.name = String::from(root);
        let mut items = vec![Item {
            rel: Vec::new(),
            dir: top,
            data: Data::None,
        }];
        let mut problems = Vec::new();
        let ok = proto::read(
            &text,
            root,
            &proto::Namespace,
            &mut |_, line, msg| problems.push(format!("{file}:{line}: {msg}")),
            &mut |f| {
                let rel: Vec<String> = f
                    .new
                    .split('/')
                    .filter(|e| !e.is_empty())
                    .map(String::from)
                    .collect();
                let mut dir = f.dir.clone();
                dir.name = rel.last().cloned().unwrap_or_default();
                if let Some(u) = &f.uid {
                    dir.uid = u.clone();
                }
                if let Some(g) = &f.gid {
                    dir.gid = g.clone();
                }
                let data = if dir.mode & DMDIR != 0 {
                    Data::None
                } else {
                    Data::File(f.old.clone())
                };
                items.push(Item { rel, dir, data });
            },
        );
        for p in problems {
            self.warn(&p);
        }
        if !ok {
            return Err(format!("{file}: bad proto file"));
        }
        Ok(Tree {
            root: String::from(root),
            items,
        })
    }
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
    } else if m & purg_styx::DMAUTH != 0 {
        'A'
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
        // A string is walked, a command run and its result walked.
        v @ (Value::Str(_) | Value::Cmd(_)) => {
            let t = fs.tree("fs", v).map_err(Error)?;
            print(&t.entries()).map_err(Error)?;
        }
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
