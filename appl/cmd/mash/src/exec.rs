//! Running commands. Builtins, functions, assignments and control flow run
//! in this prog; other commands are spawned. A stage of a pipeline, a
//! background job or a `<{...}` generator that is not a plain command runs
//! in a sub-mash: `mash -c` with a preamble that recreates this mash's
//! variables, functions and loaded modules, where Inferno's mash would
//! spawn a thread sharing them.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

use purgatorio::abi::{error, mode, pctl};
use purgatorio::{Error, Waitmsg};

use crate::ast::{Body, Cmd, Ctx, Expr, Gen, RKind, Redir, Word, quote};
use crate::glob;
use crate::history::History;
use crate::lex::ident;
use crate::make::Make;
use crate::parse::Parser;

pub const MASH: &str = "/bin/mash.wasm";
pub const TRUE: &str = "true";
const MAXLEVEL: usize = 256;

pub type List = Vec<String>;

/// Why a command stopped.
#[derive(Debug)]
pub enum Stop {
    /// An error was reported; the command is abandoned and the next one
    /// runs.
    Err,
    /// `exit`.
    Exit,
    /// A failure that ends mash, with its status: from `-e`, or a failed
    /// make.
    Fail(String),
}

pub type Flow<T = ()> = Result<T, Stop>;

/// Where a command's standard output or error goes: an fd of this prog,
/// or a buffer for a generator run in this prog.
#[derive(Clone)]
pub enum Sink {
    Fd(i32),
    Buf(Rc<RefCell<Vec<u8>>>),
}

impl Sink {
    pub fn write(&self, b: &[u8]) {
        match self {
            Sink::Fd(fd) => {
                // SAFETY: the fd is open for the command's duration and
                // ManuallyDrop keeps the File from closing it.
                let f = ManuallyDrop::new(unsafe { File::from_raw_fd(*fd) });
                let _ = (&*f).write_all(b);
            }
            Sink::Buf(buf) => buf.borrow_mut().extend_from_slice(b),
        }
    }

    pub fn print(&self, s: &str) {
        self.write(s.as_bytes());
    }
}

#[derive(Clone)]
pub struct Io {
    pub inp: i32,
    pub out: Sink,
    pub err: Sink,
}

impl Io {
    pub fn std() -> Io {
        Io {
            inp: 0,
            out: Sink::Fd(1),
            err: Sink::Fd(2),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Flags {
    /// `-x`: print each simple command first.
    pub echo: bool,
    /// `-e`: a failed command or reported error ends mash.
    pub raise: bool,
    /// `-d`: print each top-level command first.
    pub dump: bool,
    /// `-n`: parse only.
    pub noexec: bool,
}

/// The module that defined a builtin.
#[derive(Clone, Debug, PartialEq)]
pub enum Module {
    Builtins,
    Make,
    /// A reactor loaded from a file, by its path and handle.
    Reactor(String, i32),
}

impl Module {
    fn name(&self) -> String {
        match self {
            Module::Builtins => String::from("builtins"),
            Module::Make => String::from("make"),
            Module::Reactor(p, _) => p.clone(),
        }
    }
}

const BUILTINS: &[&str] = &[
    "cd", "env", "eval", "exit", "load", "prompt", "quote", "run", "status", "time", "whatis",
];
const MAKE: &[&str] = &["depends", "make", "match", "rules"];

/// A started prog, with the pipes that carry its output to buffers.
pub struct Started {
    pub pid: u32,
    collect: Vec<(OwnedFd, Rc<RefCell<Vec<u8>>>)>,
}

pub struct Shell {
    pub globals: BTreeMap<String, List>,
    pub locals: Option<BTreeMap<String, List>>,
    pub fns: BTreeMap<String, Rc<Body>>,
    pub builtins: BTreeMap<String, Module>,
    /// Modules loaded with `load`, in order, for a sub-mash.
    loaded: Vec<String>,
    pub flags: Flags,
    /// Top level: dependencies, rules and `make` are allowed.
    pub top: bool,
    /// The file and line being run, for error messages.
    pub where_: Option<(String, usize)>,
    /// `$1`, `$2` ... while a make rule runs.
    pub margs: Option<Vec<String>>,
    level: usize,
    background: HashSet<u32>,
    exited: HashMap<u32, String>,
    pub prompt: String,
    pub contin: String,
    pub make: Make,
    pub history: Option<History>,
    /// Pipe ends from `<{...}` and `>{...}` for the current command, and
    /// the fds they are given to it at.
    keep: Vec<OwnedFd>,
}

/// mash's `fail:` status for a child's status.
fn fail_status(s: &str) -> String {
    if s.starts_with("fail:") {
        String::from(s)
    } else {
        format!("fail:{s}")
    }
}

/// A null list: no words, or one empty word.
fn norm(v: List) -> List {
    if v.len() == 1 && v[0].is_empty() {
        Vec::new()
    } else {
        v
    }
}

/// `^`: a singleton distributes over a list; two lists join pairwise, the
/// shorter padded with empty strings. A null operand yields the other.
pub fn caret(a: (List, bool), b: (List, bool)) -> (List, bool) {
    let ((a, xa), (b, xb)) = (a, b);
    match (a.len(), b.len()) {
        (n, 0) => (a, xa && n == 1),
        (0, m) => (b, xb && m == 1),
        (1, 1) => (vec![format!("{}{}", a[0], b[0])], xa || xb),
        (1, _) => (b.iter().map(|y| format!("{}{y}", a[0])).collect(), false),
        (_, 1) => (a.iter().map(|x| format!("{x}{}", b[0])).collect(), false),
        (n, m) => (
            (0..n.max(m))
                .map(|i| {
                    let x = a.get(i).map_or("", String::as_str);
                    let y = b.get(i).map_or("", String::as_str);
                    format!("{x}{y}")
                })
                .collect(),
            false,
        ),
    }
}

/// Search for regular expression `re` in `s`.
fn re_match(s: &str, re: &str) -> bool {
    regexp::Regex::new(re).is_ok_and(|r| r.is_match(s))
}

/// `~`: a singleton against a list of expressions, or a list of strings
/// against one expression. Two lists never match.
pub fn match2(a: &[String], b: &[String]) -> bool {
    match (a, b) {
        ([s], [re]) => re_match(s, re),
        ([s], res) => res.iter().any(|re| re_match(s, re)),
        ([], [re]) => re_match("", re),
        ([], []) => true,
        ([], res) => res.iter().any(|re| re_match("", re)),
        (l, [re]) => l.iter().any(|s| re_match(s, re)),
        _ => false,
    }
}

fn truth(b: bool) -> List {
    if b {
        vec![String::from(TRUE)]
    } else {
        Vec::new()
    }
}

/// `%.4g` of a number of seconds.
pub fn fmt_g4(x: f64) -> String {
    if x == 0.0 {
        return String::from("0");
    }
    let exp = x.abs().log10().floor() as i32;
    if !(-5..4).contains(&exp) {
        let s = format!("{:.3e}", x);
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let m = if m.contains('.') {
            m.trim_end_matches('0').trim_end_matches('.')
        } else {
            m
        };
        let e: i32 = e.parse().unwrap_or(0);
        return format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    let decimals = (3 - exp).max(0) as usize;
    let s = format!("{x:.decimals$}");
    if s.contains('.') {
        String::from(s.trim_end_matches('0').trim_end_matches('.'))
    } else {
        s
    }
}

/// The module a command name runs: the name with `.wasm` added if it is
/// missing, in the current directory and then in `/bin`.
fn resolve(name: &str) -> Result<String, String> {
    let file = if name.ends_with(".wasm") {
        String::from(name)
    } else {
        format!("{name}.wasm")
    };
    let mut tries = vec![file.clone()];
    if !file.starts_with('/') && !file.starts_with("./") {
        tries.push(format!("/bin/{file}"));
    }
    for t in &tries {
        if purgatorio::stat(t).is_ok_and(|d| !d.qid.is_dir()) {
            return Ok(t.clone());
        }
    }
    Err(format!("{file}: {}", error::ENONEXIST))
}

impl Shell {
    pub fn new() -> Shell {
        let mut builtins = BTreeMap::new();
        for b in BUILTINS {
            builtins.insert(String::from(*b), Module::Builtins);
        }
        Shell {
            globals: BTreeMap::new(),
            locals: None,
            fns: BTreeMap::new(),
            builtins,
            loaded: Vec::new(),
            flags: Flags::default(),
            top: true,
            where_: None,
            margs: None,
            level: 0,
            background: HashSet::new(),
            exited: HashMap::new(),
            prompt: String::from("mash% "),
            contin: String::from("\t"),
            make: Make::default(),
            history: None,
            keep: Vec::new(),
        }
    }

    /// Reports an error, prefixed with the file and line when reading a
    /// file. Under `-e` this ends mash.
    pub fn report(&self, io: &Io, msg: &str) -> Stop {
        let w = match &self.where_ {
            Some((f, l)) => format!("{f}:{l}: "),
            None => String::new(),
        };
        io.err.print(&format!("{w}{msg}\n"));
        if self.flags.raise {
            Stop::Fail(String::from("fail: mash error"))
        } else {
            Stop::Err
        }
    }

    fn usage(&self, io: &Io, s: &str) -> Stop {
        self.report(io, &format!("usage: {s}"))
    }

    pub fn lookup(&self, name: &str) -> Option<&List> {
        if let Some(v) = self.locals.as_ref().and_then(|l| l.get(name)) {
            return Some(v);
        }
        self.globals.get(name)
    }

    /// `name = value`: a local if there is one, else a global.
    pub fn set(&mut self, name: &str, v: List) {
        if let Some(l) = self.locals.as_mut()
            && l.contains_key(name)
        {
            l.insert(String::from(name), v);
            return;
        }
        self.globals.insert(String::from(name), v);
    }

    /// `name := value`.
    pub fn let_(&mut self, name: &str, v: List) {
        self.locals
            .get_or_insert_default()
            .insert(String::from(name), v);
    }

    /// Runs `src` a command at a time. `name` is the file it came from,
    /// for error messages; `None` when typed interactively.
    pub fn run_source(&mut self, src: &str, name: Option<&str>, io: &Io) -> Flow {
        let mut p = Parser::new(src);
        let saved = self.where_.take();
        let r = self.run_parser(&mut p, name, io);
        self.where_ = saved;
        r
    }

    fn run_parser(&mut self, p: &mut Parser, name: Option<&str>, io: &Io) -> Flow {
        loop {
            let next = p.command();
            self.where_ = name.map(|n| (String::from(n), p.line()));
            match next {
                Ok(None) => return Ok(()),
                Ok(Some((c, bg))) => {
                    if !p.errs.is_empty() {
                        for e in std::mem::take(&mut p.errs) {
                            stop_unless_err(self.report(io, &e))?;
                        }
                        continue;
                    }
                    if self.flags.dump {
                        io.out
                            .print(&format!("{c}{}\n", if bg { '&' } else { ';' }));
                    }
                    if self.flags.noexec {
                        continue;
                    }
                    self.item(&c, bg, io)?;
                }
                Err(msg) => {
                    for e in std::mem::take(&mut p.errs) {
                        stop_unless_err(self.report(io, &e))?;
                    }
                    stop_unless_err(self.report(io, &msg))?;
                    p.recover();
                }
            }
        }
    }

    /// Runs one command of a sequence; a reported error does not stop the
    /// sequence.
    pub fn item(&mut self, c: &Cmd, bg: bool, io: &Io) -> Flow {
        let r = if bg {
            self.background_cmd(c, io)
        } else {
            self.run(c, io)
        };
        match r {
            Err(Stop::Err) => Ok(()),
            r => r,
        }
    }

    pub fn run_body(&mut self, body: &Body, io: &Io) -> Flow {
        for (c, bg) in body {
            self.item(c, *bg, io)?;
        }
        Ok(())
    }

    pub fn run(&mut self, c: &Cmd, io: &Io) -> Flow {
        match c {
            Cmd::Simple { words, redirs } => self.simple(words, redirs, io),
            Cmd::Group {
                body,
                redirs,
                scope,
            } => {
                let (rio, _files) = self.redirect(redirs, io)?;
                if *scope {
                    self.in_clone(|s| s.run_body(body, &rio))
                } else {
                    self.run_body(body, &rio)
                }
            }
            Cmd::Pipe(stages) => self.pipeline(stages, io),
            Cmd::For { var, list, body } => {
                let values = self.words(list, io)?;
                for v in values {
                    self.let_(var, vec![v]);
                    self.item(body, false, io)?;
                }
                Ok(())
            }
            Cmd::If { cond, then, els } => {
                if self.truth(cond, io)? {
                    self.item(then, false, io)
                } else if let Some(e) = els {
                    self.item(e, false, io)
                } else {
                    Ok(())
                }
            }
            Cmd::While { cond, body } => {
                while self.truth(cond, io)? {
                    self.item(body, false, io)?;
                }
                Ok(())
            }
            Cmd::Case { subject, arms } => {
                let s = self.expr_or_null(subject, io)?;
                for (pat, c, bg) in arms {
                    let p = self.expr_or_null(pat, io)?;
                    if match2(&s, &p) {
                        return self.item(c, *bg, io);
                    }
                }
                Ok(())
            }
            Cmd::Assign { name, local, value } => {
                let v = self.words(value, io)?;
                if *local {
                    self.let_(name, v);
                } else {
                    self.set(name, v);
                }
                Ok(())
            }
            Cmd::Fn { name, body } => {
                let (n, _) = self.word(name, io)?;
                let [n] = n.as_slice() else {
                    return Err(self.report(io, "bad function name"));
                };
                if !ident(n) {
                    return Err(self.report(io, "bad function name"));
                }
                match body {
                    Some(b) => {
                        self.fns.insert(n.clone(), b.clone());
                    }
                    None => {
                        self.fns.remove(n);
                    }
                }
                Ok(())
            }
            Cmd::Rescue { .. } => Err(self.report(io, "rescue not implemented")),
            Cmd::Depend {
                targets,
                deps,
                recipe,
            } => self.depend(targets, deps, recipe.as_ref(), io),
            Cmd::Rule {
                pattern,
                dep,
                recipe,
            } => self.rule(pattern, dep, recipe, io),
        }
    }

    /// Runs `f` with a copy of the variables and functions, which it
    /// cannot change, and not at top level: `@{...}`, as a cloned
    /// environment in mash.
    fn in_clone<T>(&mut self, f: impl FnOnce(&mut Shell) -> T) -> T {
        let g = self.globals.clone();
        let l = self.locals.clone();
        let fns = self.fns.clone();
        let top = std::mem::replace(&mut self.top, false);
        let r = f(self);
        self.globals = g;
        self.locals = l;
        self.fns = fns;
        self.top = top;
        r
    }

    fn truth(&mut self, e: &Expr, io: &Io) -> Flow<bool> {
        Ok(!self.expr_or_null(e, io)?.is_empty())
    }

    /// An expression's value, globbed; an error reads as the null list.
    fn expr_or_null(&mut self, e: &Expr, io: &Io) -> Flow<List> {
        match self.expr(e, io) {
            Ok((v, x)) => Ok(globbed(v, x)),
            Err(Stop::Err) => Ok(Vec::new()),
            Err(s) => Err(s),
        }
    }

    pub fn expr(&mut self, e: &Expr, io: &Io) -> Flow<(List, bool)> {
        Ok(match e {
            Expr::Word(w) => self.word(w, io)?,
            Expr::Caret(a, b) => {
                let a = self.expr(a, io)?;
                let b = self.expr(b, io)?;
                caret(a, b)
            }
            Expr::Hd(a) => {
                let (v, x) = self.expr(a, io)?;
                match v.len() {
                    0 | 1 => (v, x),
                    _ => (vec![v[0].clone()], false),
                }
            }
            Expr::Tl(a) => {
                let (v, _) = self.expr(a, io)?;
                (v.into_iter().skip(1).collect(), false)
            }
            Expr::Len(a) => (vec![self.expr(a, io)?.0.len().to_string()], false),
            Expr::Not(a) => (truth(self.expr(a, io)?.0.is_empty()), false),
            Expr::Cons(a, b) => {
                let mut l = self.expr(a, io)?.0;
                l.extend(self.expr(b, io)?.0);
                (l, false)
            }
            Expr::Eq(a, b) | Expr::Ne(a, b) => {
                let l = self.expr(a, io)?.0;
                let r = self.expr(b, io)?.0;
                (truth((l == r) == matches!(e, Expr::Eq(..))), false)
            }
            Expr::Match(a, b) => {
                let l = self.expr(a, io)?.0;
                let r = self.expr(b, io)?.0;
                (truth(match2(&l, &r)), false)
            }
        })
    }

    /// A word's value, and whether it is a pattern to glob.
    pub fn word(&mut self, w: &Word, io: &Io) -> Flow<(List, bool)> {
        Ok(match w {
            Word::Lit { text, glob, .. } => (norm(vec![text.clone()]), *glob),
            Word::Var(n) => match self.lookup(n) {
                Some(v) => (v.clone(), false),
                None => return Err(self.report(io, &format!("{n}: undefined"))),
            },
            Word::VarQ(n) => match self.lookup(n) {
                Some(v) => (norm(vec![v.join(" ")]), false),
                None => return Err(self.report(io, &format!("{n}: undefined"))),
            },
            Word::Arg(n) => {
                let v = n
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| self.margs.as_ref()?.get(i).cloned());
                (vec![v.unwrap_or_else(|| format!("${n}"))], false)
            }
            Word::Join(a, b) | Word::Caret(a, b) => {
                let a = self.word(a, io)?;
                let b = self.word(b, io)?;
                caret(a, b)
            }
            Word::Expr(e) => self.expr(e, io)?,
            Word::Gen(g, body) => (self.generate(*g, body, io)?, false),
        })
    }

    /// Words, globbed, as a list.
    pub fn words(&mut self, ws: &[Word], io: &Io) -> Flow<List> {
        let mut out = Vec::new();
        for w in ws {
            let (v, x) = self.word(w, io)?;
            out.extend(globbed(v, x));
        }
        Ok(out)
    }

    fn generate(&mut self, g: Gen, body: &Rc<Body>, io: &Io) -> Flow<List> {
        match g {
            Gen::Backq | Gen::Quote => {
                let buf = Rc::new(RefCell::new(Vec::new()));
                let gio = Io {
                    out: Sink::Buf(buf.clone()),
                    ..io.clone()
                };
                self.run_body(body, &gio)?;
                let text = String::from_utf8_lossy(&buf.borrow()).into_owned();
                Ok(if g == Gen::Backq {
                    text.split([' ', '\t', '\r', '\n'])
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                } else {
                    let line = text.split('\n').next().unwrap_or("");
                    norm(vec![String::from(line)])
                })
            }
            Gen::In | Gen::Out => {
                let [r, w] = purgatorio::pipe()
                    .map_err(|e| self.report(io, &format!("pipe failed: {e}")))?;
                let (ours, theirs) = if g == Gen::In { (r, w) } else { (w, r) };
                let mut gio = io.clone();
                if g == Gen::In {
                    gio.out = Sink::Fd(theirs.as_raw_fd());
                } else {
                    gio.inp = theirs.as_raw_fd();
                }
                // The generator gets none of the other generators' pipes.
                // One command runs without a sub-mash.
                let kept = std::mem::take(&mut self.keep);
                let started = match body.as_slice() {
                    [(c @ Cmd::Simple { .. }, false)] => self.stage(c, &gio, false),
                    _ => self
                        .start_mash(&crate::ast::body_text(body), &gio)
                        .map(Some),
                };
                self.keep = kept;
                drop(theirs);
                if let Some(s) = started? {
                    self.background.insert(s.pid);
                }
                let name = format!("/fd/{}", ours.as_raw_fd());
                self.keep.push(ours);
                Ok(vec![name])
            }
        }
    }

    /// Opens redirections and points `io` at them. The fds must outlive
    /// the command's use of the returned `Io`.
    fn redirect(&mut self, redirs: &[Redir], io: &Io) -> Flow<(Io, Vec<OwnedFd>)> {
        let mut names = Vec::new();
        for r in redirs {
            let (v, _) = self.word(&r.word, io)?;
            match v.as_slice() {
                [s] => names.push((r.kind, s.clone())),
                [] => return Err(self.report(io, "null redirect")),
                _ => return Err(self.report(io, "list for redirect")),
            }
        }
        let mut io = io.clone();
        let mut files = Vec::new();
        for (kind, name) in names {
            let fd = match kind {
                RKind::In => purgatorio::open(&name, mode::OREAD),
                RKind::InOut => purgatorio::open(&name, mode::ORDWR),
                RKind::Out => purgatorio::create(&name, mode::OWRITE, 0o666),
                RKind::Append => match purgatorio::open(&name, mode::OWRITE) {
                    Ok(fd) => {
                        let mut f = File::from(fd);
                        let _ = f.seek(SeekFrom::End(0));
                        Ok(OwnedFd::from(f))
                    }
                    Err(_) => purgatorio::create(&name, mode::OWRITE, 0o666),
                },
            };
            let fd = match fd {
                Ok(fd) => fd,
                Err(e) => {
                    io.err.print(&format!("{name}: {e}\n"));
                    return Err(Stop::Err);
                }
            };
            match kind {
                RKind::In => io.inp = fd.as_raw_fd(),
                RKind::Out | RKind::Append => io.out = Sink::Fd(fd.as_raw_fd()),
                RKind::InOut => {
                    io.inp = fd.as_raw_fd();
                    io.out = Sink::Fd(fd.as_raw_fd());
                }
            }
            files.push(fd);
        }
        Ok((io, files))
    }

    fn simple(&mut self, words: &[Word], redirs: &[Redir], io: &Io) -> Flow {
        let saved = std::mem::take(&mut self.keep);
        let r = self.simple1(words, redirs, io);
        self.keep = saved;
        r
    }

    fn simple1(&mut self, words: &[Word], redirs: &[Redir], io: &Io) -> Flow {
        let argv = self.words(words, io)?;
        if argv.is_empty() {
            return Ok(());
        }
        if self.flags.echo {
            io.out.print(&format!("+ {}\n", argv.join(" ")));
        }
        let (rio, _files) = self.redirect(redirs, io)?;
        self.runit(&argv, &rio)
    }

    pub fn internal(&self, name: &str) -> bool {
        self.fns.contains_key(name) || self.builtins.contains_key(name)
    }

    /// Runs a function, builtin or command and waits for it.
    pub fn runit(&mut self, argv: &[String], io: &Io) -> Flow {
        if let Some(body) = self.fns.get(&argv[0]).cloned() {
            if self.level >= MAXLEVEL {
                return Err(self.report(io, &format!("{}: function nesting too deep", argv[0])));
            }
            let saved = self
                .locals
                .replace(BTreeMap::from([(String::from("args"), argv[1..].to_vec())]));
            self.level += 1;
            let r = self.run_body(&body, io);
            self.level -= 1;
            self.locals = saved;
            return r;
        }
        if let Some(m) = self.builtins.get(&argv[0]).cloned() {
            return self.builtin(&m, argv, io);
        }
        let Some(started) = self.start_cmd(argv, io) else {
            return Ok(());
        };
        let pid = started.pid;
        self.finish(started);
        let status = self.wait_for(&[pid]).pop().unwrap_or_default();
        if self.flags.raise && !status.is_empty() {
            return Err(Stop::Fail(fail_status(&status)));
        }
        Ok(())
    }

    /// Starts a command, or prints why it cannot start, as mash does.
    fn start_cmd(&mut self, argv: &[String], io: &Io) -> Option<Started> {
        let path = match resolve(&argv[0]) {
            Ok(p) => p,
            Err(e) => {
                io.err.print(&format!("{e}\n"));
                return None;
            }
        };
        match self.start(&path, argv, io) {
            Ok(s) => Some(s),
            Err(e) => {
                io.err.print(&format!("{}: {e}\n", argv[0]));
                None
            }
        }
    }

    /// Spawns `path`. Standard output or error going to a buffer goes
    /// through a pipe that [`Shell::finish`] reads.
    pub fn start(&mut self, path: &str, argv: &[String], io: &Io) -> Result<Started, Error> {
        let mut collect = Vec::new();
        let mut writers = Vec::new();
        let mut fdmap = vec![(io.inp, 0)];
        for (sink, n) in [(&io.out, 1), (&io.err, 2)] {
            match sink {
                Sink::Fd(fd) => fdmap.push((*fd, n)),
                Sink::Buf(b) => {
                    let [r, w] = purgatorio::pipe()?;
                    fdmap.push((w.as_raw_fd(), n));
                    writers.push(w);
                    collect.push((r, b.clone()));
                }
            }
        }
        for k in &self.keep {
            fdmap.push((k.as_raw_fd(), k.as_raw_fd()));
        }
        let pid = purgatorio::spawn(path, argv, pctl::NEWFD, &fdmap)?;
        drop(writers);
        Ok(Started { pid, collect })
    }

    /// Reads a started prog's buffered output to its end.
    pub fn finish(&mut self, s: Started) {
        for (r, buf) in s.collect {
            let mut data = Vec::new();
            let _ = File::from(r).read_to_end(&mut data);
            buf.borrow_mut().extend_from_slice(&data);
        }
    }

    /// The text that recreates this mash's state in a sub-mash.
    fn preamble(&self) -> String {
        let mut s = String::new();
        for m in &self.loaded {
            s.push_str(&format!("load {};\n", quote(m)));
        }
        let assign = |s: &mut String, k: &str, eq: &str, v: &List| {
            s.push_str(k);
            s.push(' ');
            s.push_str(eq);
            for x in v {
                s.push(' ');
                s.push_str(&quote(x));
            }
            s.push_str(";\n");
        };
        for (k, v) in &self.globals {
            assign(&mut s, k, "=", v);
        }
        if let Some(l) = &self.locals {
            for (k, v) in l {
                assign(&mut s, k, ":=", v);
            }
        }
        for (k, b) in &self.fns {
            let f = Cmd::Fn {
                name: Word::lit(k, false),
                body: Some(b.clone()),
            };
            s.push_str(&format!("{f};\n"));
        }
        s
    }

    /// Starts a sub-mash running `src`.
    fn start_mash(&mut self, src: &str, io: &Io) -> Flow<Started> {
        let mut argv = vec![String::from("mash")];
        if self.flags.echo {
            argv.push(String::from("-x"));
        }
        if self.flags.raise {
            argv.push(String::from("-e"));
        }
        argv.push(String::from("-c"));
        argv.push(format!("{}{src}", self.preamble()));
        self.start(MASH, &argv, io)
            .map_err(|e| self.report(io, &format!("mash: {e}")))
    }

    fn cmd_text(&self, c: &Cmd) -> String {
        c.text_with(&Ctx {
            args: self.margs.as_deref(),
        })
    }

    /// Starts one stage of a pipeline or a background command, or, for a
    /// plain function or builtin call that `run_here` allows, runs it.
    fn stage(&mut self, c: &Cmd, io: &Io, run_here: bool) -> Flow<Option<Started>> {
        let Cmd::Simple { words, redirs } = c else {
            if run_here {
                self.in_clone(|s| s.item(c, false, io))?;
                return Ok(None);
            }
            let src = self.cmd_text(c);
            return self.start_mash(&src, io).map(Some);
        };
        let argv = self.words(words, io)?;
        if argv.is_empty() {
            return Ok(None);
        }
        if self.flags.echo {
            io.out.print(&format!("+ {}\n", argv.join(" ")));
        }
        let (rio, _files) = self.redirect(redirs, io)?;
        if !self.internal(&argv[0]) {
            return Ok(self.start_cmd(&argv, &rio));
        }
        if run_here {
            self.in_clone(|s| match s.runit(&argv, &rio) {
                Err(Stop::Err) => Ok(()),
                r => r,
            })?;
            return Ok(None);
        }
        let src: Vec<String> = argv.iter().map(|a| quote(a)).collect();
        self.start_mash(&src.join(" "), &rio).map(Some)
    }

    fn pipeline(&mut self, stages: &[Cmd], io: &Io) -> Flow {
        let saved = std::mem::take(&mut self.keep);
        let r = self.pipeline1(stages, io);
        self.keep = saved;
        r
    }

    fn pipeline1(&mut self, stages: &[Cmd], io: &Io) -> Flow {
        let n = stages.len();
        let mut started = Vec::new();
        let mut input: Option<OwnedFd> = None;
        let mut result = Ok(());
        for (i, c) in stages.iter().enumerate() {
            let mut sio = io.clone();
            if let Some(r) = &input {
                sio.inp = r.as_raw_fd();
            }
            let last = i + 1 == n;
            let pipe = if last {
                None
            } else {
                match purgatorio::pipe() {
                    Ok([r, w]) => {
                        sio.out = Sink::Fd(w.as_raw_fd());
                        Some((r, w))
                    }
                    Err(e) => {
                        result = Err(self.report(io, &format!("pipe failed: {e}")));
                        break;
                    }
                }
            };
            match self.stage(c, &sio, last) {
                Ok(Some(s)) => started.push(s),
                Ok(None) | Err(Stop::Err) => {}
                Err(s) => {
                    result = Err(s);
                    break;
                }
            }
            input = pipe.map(|(r, _w)| r);
        }
        drop(input);
        let mut pids = Vec::new();
        for s in started {
            pids.push(s.pid);
            self.finish(s);
        }
        let statuses = self.wait_for(&pids);
        result?;
        if self.flags.raise
            && let Some(s) = statuses.iter().rev().find(|s| !s.is_empty())
        {
            return Err(Stop::Fail(fail_status(s)));
        }
        Ok(())
    }

    /// `cmd &`: runs with standard input from `/dev/null`, unwaited for.
    fn background_cmd(&mut self, c: &Cmd, io: &Io) -> Flow {
        let null = purgatorio::open("/dev/null", mode::OREAD)
            .map_err(|e| self.report(io, &format!("could not open /dev/null: {e}")))?;
        let bio = Io {
            inp: null.as_raw_fd(),
            ..io.clone()
        };
        let saved = std::mem::take(&mut self.keep);
        let top = std::mem::replace(&mut self.top, false);
        let r = self.stage(c, &bio, false);
        self.top = top;
        self.keep = saved;
        if let Some(s) = r? {
            self.background.insert(s.pid);
            self.finish(s);
        }
        Ok(())
    }

    /// Waits until every prog in `pids` has exited and returns their
    /// statuses in order. Background progs are reaped along the way.
    pub fn wait_for(&mut self, pids: &[u32]) -> Vec<String> {
        while !pids.iter().all(|p| self.exited.contains_key(p)) {
            match purgatorio::wait() {
                Ok(Waitmsg { pid, status, .. }) => {
                    if !self.background.remove(&pid) {
                        self.exited.insert(pid, status);
                    }
                }
                Err(_) => break,
            }
        }
        pids.iter()
            .map(|p| self.exited.remove(p).unwrap_or_default())
            .collect()
    }

    fn builtin(&mut self, m: &Module, argv: &[String], io: &Io) -> Flow {
        match m {
            Module::Builtins => self.core(argv, io),
            Module::Make => self.make_cmd(argv, io),
            Module::Reactor(_, h) => {
                let mut args = Vec::new();
                for a in argv {
                    args.extend_from_slice(a.as_bytes());
                    args.push(0);
                }
                match purgatorio::call(*h, &argv[0], &args) {
                    Ok(out) => {
                        io.out.write(&out);
                        Ok(())
                    }
                    Err(e) => Err(self.report(io, &format!("{}: {e}", argv[0]))),
                }
            }
        }
    }

    fn prone(&self, io: &Io, name: &str, eq: &str, v: &List) {
        let mut s = format!("{name} {eq}");
        for x in v {
            s.push(' ');
            s.push_str(&quote(x));
        }
        s.push_str(";\n");
        io.out.print(&s);
    }

    fn core(&mut self, argv: &[String], io: &Io) -> Flow {
        let args = &argv[1..];
        match argv[0].as_str() {
            "cd" => {
                let dir = match args {
                    [] => self
                        .lookup("home")
                        .and_then(|h| h.first().cloned())
                        .unwrap_or_else(|| String::from("/")),
                    [d] => d.clone(),
                    _ => return Err(self.usage(io, "cd [dir]")),
                };
                purgatorio::chdir(&dir).map_err(|e| self.report(io, &format!("cd: {dir}: {e}")))
            }
            "env" => {
                if !args.is_empty() {
                    return Err(self.usage(io, "env"));
                }
                for (k, v) in &self.globals {
                    self.prone(io, k, "=", v);
                }
                for (k, v) in self.locals.iter().flatten() {
                    self.prone(io, k, ":=", v);
                }
                Ok(())
            }
            "eval" => {
                let src: String = args.iter().map(|a| format!(" {a}")).collect();
                self.run_source(&src, Some("<string>"), io)
            }
            "exit" => Err(Stop::Exit),
            "load" => match args {
                [f] => self.load(f, io),
                _ => Err(self.usage(io, "load file")),
            },
            "prompt" => {
                match args {
                    [] => io.out.print(&self.prompt),
                    [p] => self.prompt = p.clone(),
                    [p, c] => {
                        self.prompt = p.clone();
                        self.contin = c.clone();
                    }
                    _ => return Err(self.usage(io, "prompt [string]")),
                }
                Ok(())
            }
            "quote" => {
                if !args.is_empty() {
                    let q: Vec<String> = args.iter().map(|a| quote(a)).collect();
                    io.out.print(&format!("{}\n", q.join(" ")));
                }
                Ok(())
            }
            "run" => self.run_cmd(args, io),
            "status" => {
                if args.is_empty() {
                    return Err(self.usage(io, "status cmd [arg ...]"));
                }
                let buf = Rc::new(RefCell::new(Vec::new()));
                let sio = Io {
                    err: Sink::Buf(buf.clone()),
                    ..io.clone()
                };
                match self.runit(args, &sio) {
                    Ok(()) | Err(Stop::Err) => {}
                    Err(s) => return Err(s),
                }
                if buf.borrow().is_empty() {
                    io.out.print(TRUE);
                }
                Ok(())
            }
            "time" => {
                if args.is_empty() {
                    return Err(self.usage(io, "time cmd [arg ...]"));
                }
                let t = std::time::Instant::now();
                let r = self.runit(args, io);
                io.err
                    .print(&format!("{}\n", fmt_g4(t.elapsed().as_secs_f64())));
                r
            }
            "whatis" => {
                for n in args {
                    self.whatis(n, io);
                }
                Ok(())
            }
            _ => unreachable!("not a core builtin"),
        }
    }

    fn whatis(&self, n: &str, io: &Io) {
        let mut found = false;
        if let Some(v) = self.globals.get(n) {
            self.prone(io, n, "=", v);
            found = true;
        }
        if let Some(b) = self.fns.get(n) {
            io.out
                .print(&format!("fn {n} {{ {} }};\n", crate::ast::body_text(b)));
            found = true;
        }
        if let Some(m) = self.builtins.get(n) {
            io.out.print(&format!("load {}; {n};\n", m.name()));
            found = true;
        }
        if let Some(v) = self.locals.as_ref().and_then(|l| l.get(n)) {
            self.prone(io, n, ":=", v);
            found = true;
        }
        if !found {
            io.out.print(&format!("{n}: not found\n"));
        }
    }

    /// `run [-] [-denx] file [arg ...]`.
    fn run_cmd(&mut self, args: &[String], io: &Io) -> Flow {
        let usage = "run [-] [-denx] file [arg ...]";
        let mut quiet = false;
        let mut flags = self.flags;
        let mut i = 0;
        while let Some(a) = args.get(i).filter(|a| a.starts_with('-')) {
            if a == "-" {
                quiet = true;
            } else {
                for c in a[1..].chars() {
                    match c {
                        'd' => flags.dump = true,
                        'e' => flags.raise = true,
                        'n' => flags.noexec = true,
                        'x' => flags.echo = true,
                        _ => return Err(self.usage(io, usage)),
                    }
                }
            }
            i += 1;
        }
        let Some(file) = args.get(i) else {
            return Err(self.usage(io, usage));
        };
        let mut path = file.clone();
        let mut src = std::fs::read_to_string(&path);
        if src.is_err() && !path.starts_with('/') && !path.starts_with("./") {
            let lib = format!("/lib/mash/{path}");
            if let Ok(s) = std::fs::read_to_string(&lib) {
                src = Ok(s);
                path = lib;
            }
        }
        let src = match src {
            Ok(s) => s,
            Err(e) => {
                if quiet {
                    return Ok(());
                }
                return Err(self.report(io, &format!("{path}: {}", Error::from(e))));
            }
        };
        let saved_locals = self.locals.replace(BTreeMap::from([(
            String::from("args"),
            args[i + 1..].to_vec(),
        )]));
        let saved_flags = std::mem::replace(&mut self.flags, flags);
        let r = self.run_source(&src, Some(&path), io);
        self.flags = saved_flags;
        self.locals = saved_locals;
        r
    }

    /// `load file`.
    fn load(&mut self, name: &str, io: &Io) -> Flow {
        match name {
            "builtins" => return Ok(()),
            "make" => {
                for b in MAKE {
                    self.builtins.insert(String::from(*b), Module::Make);
                }
            }
            "history" => {
                if self.history.is_none() {
                    match History::start() {
                        Ok(h) => {
                            self.background.extend(&h.pids);
                            self.history = Some(h);
                        }
                        Err(e) => return Err(self.report(io, &format!("history: {e}"))),
                    }
                }
            }
            "tk" => {
                return Err(self.report(
                    io,
                    "tk: the Tk builtins are not available until Purgatorio has Tk",
                ));
            }
            _ => return self.load_reactor(name, io),
        }
        if !self.loaded.iter().any(|m| m == name) {
            self.loaded.push(String::from(name));
        }
        Ok(())
    }

    /// Loads a reactor module of builtins. Its `mashinit` export gets the
    /// `load` command line and returns the names of the builtins it
    /// defines; each builtin is an export of that name, given the
    /// command's words, each ended by a NUL, and returning its output.
    fn load_reactor(&mut self, name: &str, io: &Io) -> Flow {
        let file = if name.ends_with(".wasm") {
            String::from(name)
        } else {
            format!("{name}.wasm")
        };
        let mut tries = vec![file.clone()];
        if !file.starts_with('/') && !file.starts_with("./") {
            tries.push(format!("/bin/lib/mash/{file}"));
            tries.push(format!("/bin/lib/{file}"));
        }
        let mut last = Error::from(error::ENONEXIST);
        let mut handle = None;
        for t in &tries {
            if purgatorio::stat(t).is_err() {
                continue;
            }
            match purgatorio::load(t) {
                Ok(h) => {
                    handle = Some(h);
                    break;
                }
                Err(e) => last = e,
            }
        }
        let Some(h) = handle else {
            return Err(self.report(io, &format!("{name}: {last}")));
        };
        let names = purgatorio::call(h, "mashinit", format!("load {name}").as_bytes())
            .map_err(|e| self.report(io, &format!("{name}: {e}")))?;
        for b in String::from_utf8_lossy(&names).split_whitespace() {
            self.builtins
                .insert(String::from(b), Module::Reactor(String::from(name), h));
        }
        if !self.loaded.iter().any(|m| m == name) {
            self.loaded.push(String::from(name));
        }
        Ok(())
    }
}

/// A word's value after file name globbing.
fn globbed(v: List, x: bool) -> List {
    match v.as_slice() {
        [s] if x => glob::expand(s),
        _ => v,
    }
}

fn stop_unless_err(s: Stop) -> Flow {
    match s {
        Stop::Err => Ok(()),
        s => Err(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> List {
        s.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn carets() {
        let c = |a: &[&str], b: &[&str]| caret((v(a), false), (v(b), false)).0;
        assert_eq!(c(&["a"], &["b"]), ["ab"]);
        assert_eq!(c(&["a"], &["1", "2"]), ["a1", "a2"]);
        assert_eq!(c(&["x", "y"], &[".c"]), ["x.c", "y.c"]);
        assert_eq!(c(&["x", "y", "z"], &["1", "2"]), ["x1", "y2", "z"]);
        assert_eq!(c(&[], &["1", "2"]), ["1", "2"]);
        assert_eq!(c(&["a"], &[]), ["a"]);
        assert!(caret((v(&["/tmp/"]), false), (v(&["*.c"]), true)).1);
    }

    #[test]
    fn matching() {
        assert!(match2(&v(&["hello"]), &v(&["^h.*o$"])));
        assert!(match2(&v(&["hello"]), &v(&["x", "ell"])));
        assert!(match2(&v(&["a", "bc"]), &v(&["c$"])));
        assert!(!match2(&v(&["a", "b"]), &v(&["a", "b"])));
        assert!(match2(&v(&[]), &v(&[])));
        assert!(match2(&v(&[]), &v(&["^$"])));
    }

    #[test]
    fn numbers() {
        assert_eq!(fmt_g4(0.0), "0");
        assert_eq!(fmt_g4(0.012), "0.012");
        assert_eq!(fmt_g4(1.5), "1.5");
        assert_eq!(fmt_g4(12.3456), "12.35");
        assert_eq!(fmt_g4(2.0), "2");
        assert_eq!(fmt_g4(123456.0), "1.235e+05");
    }
}
