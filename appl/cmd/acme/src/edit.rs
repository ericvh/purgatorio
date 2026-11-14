//! `Edit`: sam(1)'s command language, as acme(1) runs it on windows'
//! bodies.
//!
//! A command line is parsed into [`Cmd`]s and each top-level command is
//! run in turn. While a command runs, the files do not change: the
//! changes it makes are logged against the text as it was, in rune
//! offsets, and applied when it ends (so addresses inside an `x` loop
//! all refer to the original text, as in sam). Changes to one file must
//! not overlap (`changes not in sequence`). The next top-level command
//! sees the changed text.
//!
//! Addresses are those of [`crate::addr`], with `"re"` before one to
//! choose the file whose menu line matches. The commands:
//!
//! - text: `a/text/`, `c/text/`, `i/text/` (or the text on the lines
//!   after the command, ending with a line holding only `.`), `d`,
//!   `s/re/text/` (`sN/re/text/` for the Nth match, `g` after for all;
//!   `&` and `\1`..`\9` in the text are the match and its parts), `m
//!   addr` and `t addr` (move or copy dot after addr);
//! - display: `p` (dot's text), `=` (dot's line address, `=#` its rune
//!   address), as `name:addr`;
//! - loops and conditions: `x/re/ cmd` (for each match in dot), `y/re/
//!   cmd` (for each piece between matches), `g/re/ cmd` and `v/re/ cmd`
//!   (if dot matches, or does not), `X/re/ cmd` and `Y/re/ cmd` (for each
//!   file whose menu line matches, or does not); `{ ... }` runs several
//!   commands with the same dot;
//! - files: `b file`, `B file ...`, `D [file ...]`, `e file`, `r file`,
//!   `w [file]`, `f [name]`;
//! - others: `k` (set the mark `'`), `u [n]` (undo n changes; a negative
//!   n redoes), `q` (does nothing), `<cmd`, `|cmd`, `>cmd` and `!cmd`
//!   (replace dot with cmd's output, pipe dot through cmd, send dot to
//!   cmd, or just run it; the output of the last two is printed).
//!
//! Everything acme does for a command goes through [`Host`].

use std::collections::HashMap;

use regexp::Regex;

use crate::addr::{self, Addr, Hay, Range};

pub const ESEQ: &str = "changes not in sequence";
pub const ENOFILE: &str = "no current file";
pub const ENOSUB: &str = "no substitution";
pub const EDELIM: &str = "bad delimiter";
pub const EBRACE: &str = "right brace expected";
pub const ENOREGEXP: &str = "no regular expression";
pub const EMOVE: &str = "move overlaps itself";
pub const ENOMATCH: &str = "no file matches";
pub const EMANY: &str = "too many files match";

/// What `Edit` needs from acme.
pub trait Host {
    /// The files: each window's body, once for windows that share one.
    fn files(&self) -> Vec<FileInfo>;
    fn text(&self, id: u64) -> Vec<char>;
    fn dot(&self, id: u64) -> Range;
    fn set_dot(&mut self, id: u64, r: Range);
    /// Replaces runes `q0..q1` of the body of `id` with `s`.
    fn replace(&mut self, id: u64, q0: usize, q1: usize, s: &str);
    /// Undoes (or redoes) the body's latest change; false if none.
    fn undo(&mut self, id: u64, redo: bool) -> bool;
    fn set_name(&mut self, id: u64, name: &str);
    fn set_clean(&mut self, id: u64);
    /// The directory names are relative to, for file `id`.
    fn dir(&self, id: Option<u64>) -> String;
    /// Opens a window on `path`, or finds the one that has it.
    fn open(&mut self, path: &str) -> Result<u64, String>;
    fn close(&mut self, id: u64) -> Result<(), String>;
    fn read_file(&mut self, path: &str) -> Result<String, String>;
    fn write_file(&mut self, path: &str, s: &str) -> Result<(), String>;
    /// Runs `cmd` in `dir` with `input` on its standard input (or none)
    /// and returns what it wrote on its standard output and error.
    fn run(&mut self, dir: &str, cmd: &str, input: Option<&str>) -> String;
    /// Prints a message where acme prints errors.
    fn print(&mut self, s: &str);
}

/// A file as [`Host::files`] lists it.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub id: u64,
    pub name: String,
    pub dirty: bool,
}

/// An address with the file it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FAddr {
    /// `"re"`: the file whose menu line matches.
    pub file: Option<String>,
    pub addr: Option<Addr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `a`, `c` or `i` and the text.
    Text(char, String),
    Delete,
    Sub {
        n: usize,
        re: String,
        rep: String,
        global: bool,
    },
    /// `m` (false) or `t` (true) and where to.
    Move(bool, FAddr),
    Print,
    /// `=`, or `=#` (true).
    Eq(bool),
    Mark,
    /// `x`, `y`, `g`, `v`, `X` or `Y`, the expression and the command.
    Loop(char, Option<String>, Box<Cmd>),
    Undo(i64),
    /// `b`, `B`, `D`, `e`, `r`, `w` or `f` and its argument.
    File(char, String),
    /// `<`, `|`, `>` or `!` and the command line.
    Run(char, String),
    Group(Vec<Cmd>),
    Quit,
    /// An address alone: it sets dot.
    Nop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub addr: Option<FAddr>,
    pub kind: Kind,
}

// ---- Parsing.

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

type R<T> = Result<T, String>;

fn bad<T>(s: &str) -> R<T> {
    Err(s.to_string())
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn blanks(&mut self) {
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.i += 1;
        }
    }

    fn blanks_nl(&mut self) {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.i += 1;
        }
    }

    fn faddr(&mut self) -> R<Option<FAddr>> {
        self.blanks();
        let file = if self.peek() == Some('"') {
            self.i += 1;
            Some(addr::pattern(self.s, &mut self.i, '"'))
        } else {
            None
        };
        let a = addr::parse(self.s, &mut self.i)?;
        if file.is_none() && a.is_none() {
            return Ok(None);
        }
        Ok(Some(FAddr { file, addr: a }))
    }

    /// The rest of the line, without the newline.
    fn line(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| c != '\n') {
            self.i += 1;
        }
        let s: String = self.s[start..self.i].iter().collect();
        if self.peek() == Some('\n') {
            self.i += 1;
        }
        s
    }

    fn delim(&mut self) -> R<char> {
        match self.peek() {
            Some(c) if is_delim(c) => {
                self.i += 1;
                Ok(c)
            }
            _ => bad(EDELIM),
        }
    }

    /// The text of `a`, `c` or `i`: delimited, or the lines up to one
    /// holding only a dot.
    fn text(&mut self) -> R<String> {
        self.blanks();
        if self.peek().is_none_or(|c| c == '\n') {
            if self.peek() == Some('\n') {
                self.i += 1;
            }
            let mut out = String::new();
            while self.peek().is_some() {
                let l = self.line();
                if l == "." {
                    break;
                }
                out.push_str(&l);
                out.push('\n');
            }
            return Ok(out);
        }
        let d = self.delim()?;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.i += 1;
            if c == d {
                break;
            }
            if c == '\\'
                && let Some(n) = self.peek()
            {
                self.i += 1;
                match n {
                    'n' => out.push('\n'),
                    _ if n == d => out.push(d),
                    '\\' => out.push('\\'),
                    _ => {
                        out.push('\\');
                        out.push(n);
                    }
                }
                continue;
            }
            out.push(c);
        }
        Ok(out)
    }

    /// A delimited regular expression, when one comes next.
    fn regexp(&mut self) -> R<Option<String>> {
        self.blanks();
        match self.peek() {
            Some(c) if is_delim(c) && c != '{' && c != '}' => {
                self.i += 1;
                Ok(Some(addr::pattern(self.s, &mut self.i, c)))
            }
            _ => Ok(None),
        }
    }

    /// The commands up to the end, or up to `}` when `group`.
    fn cmds(&mut self, group: bool) -> R<Vec<Cmd>> {
        let mut out = Vec::new();
        loop {
            self.blanks_nl();
            match self.peek() {
                None if group => return bad(EBRACE),
                None => return Ok(out),
                Some('}') if group => {
                    self.i += 1;
                    return Ok(out);
                }
                Some('}') => return bad("unmatched `}'"),
                _ => {}
            }
            if let Some(c) = self.cmd()? {
                out.push(c);
            }
        }
    }

    fn cmd(&mut self) -> R<Option<Cmd>> {
        let a = self.faddr()?;
        self.blanks();
        let Some(c) = self.peek() else {
            return Ok(a.map(|a| Cmd {
                addr: Some(a),
                kind: Kind::Nop,
            }));
        };
        if c == '\n' || c == '}' {
            return Ok(a.map(|a| Cmd {
                addr: Some(a),
                kind: Kind::Nop,
            }));
        }
        self.i += 1;
        let kind = match c {
            'a' | 'c' | 'i' => Kind::Text(c, self.text()?),
            'd' => Kind::Delete,
            's' => {
                let mut n = 0usize;
                while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
                    n = n.saturating_mul(10).saturating_add(d as usize);
                    self.i += 1;
                }
                let d = self.delim()?;
                let re = addr::pattern(self.s, &mut self.i, d);
                let mut rep = String::new();
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.i += 1;
                    if c == d {
                        break;
                    }
                    if c == '\\'
                        && let Some(n) = self.peek()
                    {
                        self.i += 1;
                        if n == d && d != '&' && !d.is_ascii_digit() {
                            rep.push(d);
                        } else {
                            rep.push('\\');
                            rep.push(n);
                        }
                        continue;
                    }
                    rep.push(c);
                }
                let global = self.peek() == Some('g');
                if global {
                    self.i += 1;
                }
                Kind::Sub {
                    n: n.max(1),
                    re,
                    rep,
                    global,
                }
            }
            'm' | 't' => {
                let to = self.faddr()?.ok_or_else(|| String::from("bad address"))?;
                Kind::Move(c == 't', to)
            }
            'p' => Kind::Print,
            '=' => {
                let hash = self.peek() == Some('#');
                if hash {
                    self.i += 1;
                }
                Kind::Eq(hash)
            }
            'k' => Kind::Mark,
            'q' => Kind::Quit,
            'u' => {
                self.blanks();
                let neg = self.peek() == Some('-');
                if neg {
                    self.i += 1;
                }
                let mut n: i64 = 0;
                let mut digits = false;
                while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
                    n = n.saturating_mul(10).saturating_add(i64::from(d));
                    digits = true;
                    self.i += 1;
                }
                if !digits {
                    n = 1;
                }
                Kind::Undo(if neg { -n } else { n })
            }
            'x' | 'y' | 'g' | 'v' | 'X' | 'Y' => {
                let re = self.regexp()?;
                if re.is_none() && matches!(c, 'g' | 'v') {
                    return bad(ENOREGEXP);
                }
                self.blanks();
                let sub = match self.peek() {
                    None | Some('\n') | Some('}') => None,
                    _ => self.cmd()?,
                };
                let sub = sub.unwrap_or(Cmd {
                    addr: None,
                    kind: if matches!(c, 'X' | 'Y') {
                        Kind::File('f', String::new())
                    } else {
                        Kind::Print
                    },
                });
                Kind::Loop(c, re, Box::new(sub))
            }
            'b' | 'B' | 'D' | 'e' | 'r' | 'w' | 'f' => {
                Kind::File(c, self.line().trim().to_string())
            }
            '<' | '|' | '>' | '!' => Kind::Run(c, self.line().trim().to_string()),
            '{' => Kind::Group(self.cmds(true)?),
            _ => return Err(format!("unknown command `{c}'")),
        };
        Ok(Some(Cmd { addr: a, kind }))
    }
}

/// Whether `c` can delimit a regular expression or text.
fn is_delim(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && c != '\\'
}

/// Parses an `Edit` command line.
pub fn parse(s: &str) -> R<Vec<Cmd>> {
    let cs: Vec<char> = s.chars().collect();
    let mut p = Parser { s: &cs, i: 0 };
    p.cmds(false)
}

// ---- Running.

/// One logged change: replace `q0..q1` with `text`.
#[derive(Clone, Debug)]
struct Change {
    q0: usize,
    q1: usize,
    text: Vec<char>,
}

/// What dot becomes when a command's changes are applied.
#[derive(Clone, Copy, Debug)]
enum Dot {
    /// A range of the text before the changes.
    Orig(Range),
    /// The text a change put in, by its index in the log.
    Change(usize),
}

/// A file's text while a command runs, and what the command did to it.
struct Snap {
    hay: Hay,
    /// Dot when the command started.
    dot: Range,
    log: Vec<Change>,
    fin: Option<Dot>,
    /// Make the file clean once the changes are in (`e`).
    clean: bool,
}

/// The state of one `Edit`.
pub struct Ed<'h, H: Host> {
    h: &'h mut H,
    cur: Option<u64>,
    snaps: HashMap<u64, Snap>,
    marks: HashMap<u64, Range>,
    /// The last regular expression, for `//`.
    last_re: Option<String>,
}

/// Runs command line `s` with `cur` as the current file. Errors are
/// printed through the host and end the line; changes made by the
/// commands before stay.
pub fn run<H: Host>(h: &mut H, cur: Option<u64>, s: &str) -> Result<(), String> {
    let cmds = match parse(s) {
        Ok(c) => c,
        Err(e) => {
            h.print(&format!("Edit: {e}\n"));
            return Err(e);
        }
    };
    let mut ed = Ed {
        h,
        cur,
        snaps: HashMap::new(),
        marks: HashMap::new(),
        last_re: None,
    };
    for c in &cmds {
        let r = ed.top(c);
        if let Err(e) = r {
            ed.snaps.clear();
            ed.h.print(&format!("Edit: {e}\n"));
            return Err(e);
        }
    }
    Ok(())
}

impl<H: Host> Ed<'_, H> {
    fn top(&mut self, c: &Cmd) -> R<()> {
        let dot = match self.cur {
            Some(id) => self.snap(id).dot,
            None => (0, 0),
        };
        if matches!(c.kind, Kind::Quit) {
            return Ok(());
        }
        self.exec(c, dot)?;
        self.apply()
    }

    fn snap(&mut self, id: u64) -> &mut Snap {
        if !self.snaps.contains_key(&id) {
            let s = Snap {
                hay: Hay::new(self.h.text(id)),
                dot: self.h.dot(id),
                log: Vec::new(),
                fin: None,
                clean: false,
            };
            self.snaps.insert(id, s);
        }
        self.snaps.get_mut(&id).unwrap_or_else(|| unreachable!())
    }

    fn cur(&self) -> R<u64> {
        self.cur.ok_or_else(|| String::from(ENOFILE))
    }

    fn regex(&mut self, pat: &str) -> R<Regex> {
        let pat = if pat.is_empty() {
            self.last_re
                .clone()
                .ok_or_else(|| String::from(ENOREGEXP))?
        } else {
            self.last_re = Some(pat.to_string());
            pat.to_string()
        };
        Regex::new(&pat).map_err(|e| format!("bad regexp: {e}"))
    }

    /// The files whose menu line `re` matches.
    fn matching(&mut self, re: &Regex) -> Vec<u64> {
        let cur = self.cur;
        self.h
            .files()
            .into_iter()
            .filter(|f| re.is_match(&menu(f, cur == Some(f.id))))
            .map(|f| f.id)
            .collect()
    }

    /// Evaluates an address; `None` gives `default`.
    fn resolve(&mut self, a: Option<&FAddr>, dot: Range, default: Option<Range>) -> R<Range> {
        let Some(a) = a else {
            self.cur()?;
            return Ok(default.unwrap_or(dot));
        };
        let mut dot = dot;
        if let Some(f) = &a.file {
            let re = self.regex(f)?;
            let ids = self.matching(&re);
            match ids.as_slice() {
                [] => return bad(ENOMATCH),
                [id] => {
                    if self.cur != Some(*id) {
                        self.cur = Some(*id);
                        dot = self.snap(*id).dot;
                    }
                }
                _ => return bad(EMANY),
            }
        }
        let id = self.cur()?;
        let mark = self.marks.get(&id).copied();
        let Some(ad) = &a.addr else {
            return Ok(dot);
        };
        // An empty regexp means the last one.
        let ad = self.fill_re(ad)?;
        let s = self.snap(id);
        Ok(addr::resolve(&s.hay, dot, mark, &ad)?)
    }

    fn fill_re(&mut self, a: &Addr) -> R<Addr> {
        Ok(match a {
            Addr::Re(p, f) => {
                if p.is_empty() {
                    let p = self
                        .last_re
                        .clone()
                        .ok_or_else(|| String::from(ENOREGEXP))?;
                    Addr::Re(p, *f)
                } else {
                    Regex::new(p).map_err(|e| format!("bad regexp: {e}"))?;
                    self.last_re = Some(p.clone());
                    a.clone()
                }
            }
            Addr::Rel(l, f, r) => Addr::Rel(
                match l {
                    Some(l) => Some(Box::new(self.fill_re(l)?)),
                    None => None,
                },
                *f,
                match r {
                    Some(r) => Some(Box::new(self.fill_re(r)?)),
                    None => None,
                },
            ),
            Addr::Range(l, s, r) => Addr::Range(
                match l {
                    Some(l) => Some(Box::new(self.fill_re(l)?)),
                    None => None,
                },
                *s,
                match r {
                    Some(r) => Some(Box::new(self.fill_re(r)?)),
                    None => None,
                },
            ),
            _ => a.clone(),
        })
    }

    /// Logs a change to the current file and returns its index.
    fn change(&mut self, q0: usize, q1: usize, text: &str) -> R<usize> {
        let id = self.cur()?;
        let s = self.snap(id);
        s.log.push(Change {
            q0,
            q1,
            text: text.chars().collect(),
        });
        Ok(s.log.len() - 1)
    }

    fn set_fin(&mut self, d: Dot) -> R<()> {
        let id = self.cur()?;
        self.snap(id).fin = Some(d);
        Ok(())
    }

    fn name(&self, id: u64) -> String {
        self.h
            .files()
            .into_iter()
            .find(|f| f.id == id)
            .map(|f| f.name)
            .unwrap_or_default()
    }

    fn path(&self, name: &str) -> String {
        if name.starts_with('/') {
            return name.to_string();
        }
        let dir = self.h.dir(self.cur);
        if dir.ends_with('/') {
            format!("{dir}{name}")
        } else {
            format!("{dir}/{name}")
        }
    }

    fn exec(&mut self, c: &Cmd, dot: Range) -> R<()> {
        match &c.kind {
            Kind::Nop => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                self.set_fin(Dot::Orig(r))
            }
            Kind::Text(k, text) => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                let i = match k {
                    'a' => self.change(r.1, r.1, text)?,
                    'i' => self.change(r.0, r.0, text)?,
                    _ => self.change(r.0, r.1, text)?,
                };
                self.set_fin(Dot::Change(i))
            }
            Kind::Delete => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                if r.0 < r.1 {
                    self.change(r.0, r.1, "")?;
                }
                self.set_fin(Dot::Orig((r.0, r.0)))
            }
            Kind::Sub { n, re, rep, global } => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                let re = self.regex(re)?;
                let id = self.cur()?;
                let matches = all(&self.snap(id).hay, &re, r);
                let mut done = 0;
                let mut subs = Vec::new();
                for (k, caps) in matches.into_iter().enumerate() {
                    if k + 1 < *n || (k + 1 > *n && !global) {
                        continue;
                    }
                    let m = caps[0].unwrap_or((0, 0));
                    let text = expand(&self.snap(id).hay, &caps, rep);
                    subs.push((m, text));
                    done += 1;
                }
                if done == 0 {
                    return bad(ENOSUB);
                }
                for (m, text) in subs {
                    self.change(m.0, m.1, &text)?;
                }
                self.set_fin(Dot::Orig(r))
            }
            Kind::Move(copy, to) => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                let id = self.cur()?;
                let t = self.resolve(Some(to), r, None)?;
                if self.cur != Some(id) {
                    return bad("move to another file");
                }
                let text = self.snap(id).hay.string(r.0, r.1);
                if !copy && t.1 > r.0 && t.1 < r.1 {
                    return bad(EMOVE);
                }
                if !copy && (t.1 == r.0 || t.1 == r.1) {
                    return self.set_fin(Dot::Orig(r));
                }
                let i;
                if !copy && t.1 < r.0 {
                    i = self.change(t.1, t.1, &text)?;
                    self.change(r.0, r.1, "")?;
                } else {
                    if !copy {
                        self.change(r.0, r.1, "")?;
                    }
                    i = self.change(t.1, t.1, &text)?;
                }
                self.set_fin(Dot::Change(i))
            }
            Kind::Print => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                let id = self.cur()?;
                let s = self.snap(id).hay.string(r.0, r.1);
                self.h.print(&s);
                self.set_fin(Dot::Orig(r))
            }
            Kind::Eq(hash) => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                let id = self.cur()?;
                let name = self.name(id);
                let t = &self.snap(id).hay.t;
                let pos = if *hash {
                    if r.0 == r.1 {
                        format!("#{}", r.0)
                    } else {
                        format!("#{},#{}", r.0, r.1)
                    }
                } else {
                    let l0 = 1 + t[..r.0].iter().filter(|c| **c == '\n').count();
                    let mut l1 = 1 + t[..r.1].iter().filter(|c| **c == '\n').count();
                    // A range ending after a newline ends on that line.
                    if r.1 > r.0 && t[r.1 - 1] == '\n' {
                        l1 -= 1;
                    }
                    if l1 <= l0 {
                        format!("{l0}")
                    } else {
                        format!("{l0},{l1}")
                    }
                };
                self.h.print(&format!("{name}:{pos}\n"));
                self.set_fin(Dot::Orig(r))
            }
            Kind::Mark => {
                let r = self.resolve(c.addr.as_ref(), dot, None)?;
                let id = self.cur()?;
                self.marks.insert(id, r);
                Ok(())
            }
            Kind::Quit => Ok(()),
            Kind::Undo(n) => {
                let id = self.cur()?;
                // The changes made so far go in first.
                self.apply()?;
                for _ in 0..n.unsigned_abs() {
                    if !self.h.undo(id, *n < 0) {
                        break;
                    }
                }
                self.snaps.remove(&id);
                Ok(())
            }
            Kind::Loop(k, re, sub) => self.looped(*k, re.as_deref(), sub, c.addr.as_ref(), dot),
            Kind::Group(cmds) => {
                let r = if c.addr.is_some() {
                    self.resolve(c.addr.as_ref(), dot, None)?
                } else {
                    dot
                };
                for sub in cmds {
                    self.exec(sub, r)?;
                }
                Ok(())
            }
            Kind::File(k, arg) => self.file(*k, arg, c.addr.as_ref(), dot),
            Kind::Run(k, cmd) => {
                let r = if *k == '!' {
                    dot
                } else {
                    self.resolve(c.addr.as_ref(), dot, None)?
                };
                let dir = self.h.dir(self.cur);
                let input = match k {
                    '|' | '>' => {
                        let id = self.cur()?;
                        Some(self.snap(id).hay.string(r.0, r.1))
                    }
                    _ => None,
                };
                let out = self.h.run(&dir, cmd, input.as_deref());
                match k {
                    '<' | '|' => {
                        let i = self.change(r.0, r.1, &out)?;
                        self.set_fin(Dot::Change(i))
                    }
                    _ => {
                        self.h.print(&out);
                        Ok(())
                    }
                }
            }
        }
    }

    fn looped(
        &mut self,
        k: char,
        re: Option<&str>,
        sub: &Cmd,
        a: Option<&FAddr>,
        dot: Range,
    ) -> R<()> {
        if k == 'X' || k == 'Y' {
            let ids = match re {
                Some(p) => {
                    let re = self.regex(p)?;
                    let yes = self.matching(&re);
                    if k == 'X' {
                        yes
                    } else {
                        self.h
                            .files()
                            .into_iter()
                            .map(|f| f.id)
                            .filter(|id| !yes.contains(id))
                            .collect()
                    }
                }
                None if k == 'X' => self.h.files().into_iter().map(|f| f.id).collect(),
                None => Vec::new(),
            };
            let old = self.cur;
            for id in ids {
                // A command may have closed it.
                if !self.h.files().iter().any(|f| f.id == id) {
                    continue;
                }
                self.cur = Some(id);
                let d = self.snap(id).dot;
                self.exec(sub, d)?;
            }
            if old.is_none_or(|o| self.h.files().iter().any(|f| f.id == o)) {
                self.cur = old;
            }
            return Ok(());
        }
        let r = self.resolve(a, dot, None)?;
        let id = self.cur()?;
        let re = match re {
            Some(p) => self.regex(p)?,
            None if k == 'x' => Regex::new(".*\n").map_err(|e| e.to_string())?,
            None => return bad(ENOREGEXP),
        };
        match k {
            'g' | 'v' => {
                let hit = self.snap(id).hay.find0(&re, r.0, r.1).is_some();
                if hit == (k == 'g') {
                    self.exec(sub, r)?;
                }
                Ok(())
            }
            _ => {
                let ms: Vec<Range> = all(&self.snap(id).hay, &re, r)
                    .into_iter()
                    .filter_map(|c| c[0])
                    .collect();
                let pieces = if k == 'x' {
                    ms
                } else {
                    let mut v = Vec::new();
                    let mut p = r.0;
                    for m in ms {
                        v.push((p, m.0));
                        p = m.1;
                    }
                    v.push((p, r.1));
                    v
                };
                for p in pieces {
                    self.cur = Some(id);
                    self.exec(sub, p)?;
                }
                Ok(())
            }
        }
    }

    fn file(&mut self, k: char, arg: &str, a: Option<&FAddr>, dot: Range) -> R<()> {
        match k {
            'b' => {
                let path = self.path(arg);
                let f = self
                    .h
                    .files()
                    .into_iter()
                    .find(|f| f.name == path || f.name == arg)
                    .ok_or_else(|| format!("no such file `{arg}'"))?;
                self.cur = Some(f.id);
                Ok(())
            }
            'B' => {
                if arg.is_empty() {
                    return bad("no file name");
                }
                for n in arg.split_whitespace() {
                    let p = self.path(n);
                    let id = self.h.open(&p)?;
                    self.cur = Some(id);
                }
                Ok(())
            }
            'D' => {
                let ids: Vec<u64> = if arg.is_empty() {
                    vec![self.cur()?]
                } else {
                    let files = self.h.files();
                    let mut v = Vec::new();
                    for n in arg.split_whitespace() {
                        let p = self.path(n);
                        let f = files
                            .iter()
                            .find(|f| f.name == p || f.name == n)
                            .ok_or_else(|| format!("no such file `{n}'"))?;
                        v.push(f.id);
                    }
                    v
                };
                for id in ids {
                    self.snaps.remove(&id);
                    self.h.close(id)?;
                    if self.cur == Some(id) {
                        self.cur = None;
                    }
                }
                Ok(())
            }
            'f' => {
                let id = self.cur()?;
                if !arg.is_empty() {
                    let p = self.path(arg);
                    self.h.set_name(id, &p);
                }
                let cur = self.cur;
                if let Some(f) = self.h.files().into_iter().find(|f| f.id == id) {
                    let m = menu(&f, cur == Some(id));
                    self.h.print(&format!("{m}\n"));
                }
                Ok(())
            }
            'e' => {
                let id = self.cur()?;
                let name = if arg.is_empty() {
                    self.name(id)
                } else {
                    self.path(arg)
                };
                if name.is_empty() {
                    return bad("no file name");
                }
                let s = self.h.read_file(&name)?;
                let n = self.snap(id).hay.len();
                self.change(0, n, &s)?;
                self.set_fin(Dot::Orig((0, 0)))?;
                self.snap(id).clean = true;
                if !arg.is_empty() {
                    self.h.set_name(id, &name);
                }
                Ok(())
            }
            'r' => {
                let r = self.resolve(a, dot, None)?;
                if arg.is_empty() {
                    return bad("no file name");
                }
                let s = self.h.read_file(&self.path(arg))?;
                let i = self.change(r.0, r.1, &s)?;
                self.set_fin(Dot::Change(i))
            }
            'w' => {
                let id = self.cur()?;
                let all = (0, self.snap(id).hay.len());
                let r = self.resolve(a, dot, Some(all))?;
                let own = self.name(id);
                let name = if arg.is_empty() {
                    own.clone()
                } else {
                    self.path(arg)
                };
                if name.is_empty() {
                    return bad("no file name");
                }
                let s = self.snap(id).hay.string(r.0, r.1);
                self.h.write_file(&name, &s)?;
                if r == all && name == own && self.snap(id).log.is_empty() {
                    self.h.set_clean(id);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Applies the logged changes to each file and sets its dot.
    fn apply(&mut self) -> R<()> {
        let mut ids: Vec<u64> = self.snaps.keys().copied().collect();
        ids.sort_unstable();
        // Check every file before changing any.
        for id in &ids {
            let s = &self.snaps[id];
            let mut order: Vec<usize> = (0..s.log.len()).collect();
            order.sort_by_key(|&i| (s.log[i].q0, s.log[i].q1));
            let mut end = 0;
            for &i in &order {
                if s.log[i].q0 < end {
                    return bad(ESEQ);
                }
                end = s.log[i].q1;
            }
        }
        for id in ids {
            let Some(s) = self.snaps.remove(&id) else {
                continue;
            };
            let mut order: Vec<usize> = (0..s.log.len()).collect();
            order.sort_by_key(|&i| (s.log[i].q0, s.log[i].q1));
            // Where each change starts in the new text.
            let mut start = vec![0usize; s.log.len()];
            let mut delta: isize = 0;
            for &i in &order {
                let c = &s.log[i];
                start[i] = (c.q0 as isize + delta) as usize;
                delta += c.text.len() as isize - (c.q1 - c.q0) as isize;
            }
            let map = |p: usize, hi: bool| -> usize {
                let mut d: isize = 0;
                for &i in &order {
                    let c = &s.log[i];
                    let n = c.text.len() as isize;
                    if c.q1 < p || (c.q1 == p && (c.q0 < p || hi)) {
                        d += n - (c.q1 - c.q0) as isize;
                    } else if c.q0 < p && p < c.q1 {
                        return (c.q0 as isize + d + if hi { n } else { 0 }) as usize;
                    } else {
                        break;
                    }
                }
                (p as isize + d) as usize
            };
            let fin = match s.fin {
                Some(Dot::Orig((q0, q1))) => {
                    Some((map(q0, false), map(q1, true).max(map(q0, false))))
                }
                Some(Dot::Change(i)) => Some((start[i], start[i] + s.log[i].text.len())),
                None => None,
            };
            for &i in order.iter().rev() {
                let c = &s.log[i];
                let t: String = c.text.iter().collect();
                self.h.replace(id, c.q0, c.q1, &t);
            }
            if s.clean {
                self.h.set_clean(id);
            }
            if let Some(d) = fin {
                self.h.set_dot(id, d);
            }
        }
        Ok(())
    }
}

/// The matches of `re` in `r` of `h`: as sam finds them for `x` and
/// `s`, a null match right after a match is skipped. The text past `r`
/// is out of sight, so `$` matches at its end.
fn all(h: &Hay, re: &Regex, r: Range) -> Vec<addr::Caps> {
    let mut out = Vec::new();
    let mut p = r.0;
    let mut prev_end: Option<usize> = None;
    while p <= r.1 {
        let Some(caps) = h.find(re, p, r.1) else {
            break;
        };
        let Some(m) = caps[0] else {
            break;
        };
        if m.0 == m.1 && prev_end == Some(m.0) {
            p = m.0 + 1;
            continue;
        }
        if m.1 > m.0 {
            prev_end = Some(m.1);
            p = m.1;
        } else {
            p = m.1 + 1;
        }
        out.push(caps);
    }
    out
}

/// The replacement text of `s` for one match: `&` is the match, `\n` a
/// newline, `\1` to `\9` the parts, `\&` and `\\` themselves.
fn expand(h: &Hay, caps: &addr::Caps, rep: &str) -> String {
    let part = |k: usize| {
        caps.get(k)
            .copied()
            .flatten()
            .map(|(a, b)| h.string(a, b))
            .unwrap_or_default()
    };
    let mut out = String::new();
    let mut it = rep.chars();
    while let Some(c) = it.next() {
        match c {
            '&' => out.push_str(&part(0)),
            '\\' => match it.next() {
                Some('n') => out.push('\n'),
                Some(d @ '0'..='9') => out.push_str(&part(d as usize - '0' as usize)),
                Some(o) => out.push(o),
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}

/// A file's line in sam's menu, which `X`, `Y` and `"re"` match: `'` if
/// modified, `+` (it has a window), `.` if current, a blank, the name.
pub fn menu(f: &FileInfo, current: bool) -> String {
    format!(
        "{}+{} {}",
        if f.dirty { '\'' } else { ' ' },
        if current { '.' } else { ' ' },
        f.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files in memory, with a whole-text undo log, standing in for acme.
    #[derive(Default)]
    struct Mock {
        files: Vec<(u64, String, String, Range, bool)>,
        undo: HashMap<u64, Vec<String>>,
        redo: HashMap<u64, Vec<String>>,
        /// Texts before this Edit began, for one undo per Edit.
        started: Vec<u64>,
        out: String,
        disk: HashMap<String, String>,
        next: u64,
    }

    impl Mock {
        fn new(files: &[(&str, &str)]) -> Mock {
            let mut m = Mock {
                next: 1,
                ..Mock::default()
            };
            for (n, t) in files {
                let id = m.next;
                m.next += 1;
                m.files
                    .push((id, n.to_string(), t.to_string(), (0, 0), false));
            }
            m
        }

        fn f(&mut self, id: u64) -> &mut (u64, String, String, Range, bool) {
            self.files.iter_mut().find(|f| f.0 == id).unwrap()
        }

        fn body(&self, id: u64) -> &str {
            &self.files.iter().find(|f| f.0 == id).unwrap().2
        }

        fn dotted(&self, id: u64) -> Range {
            self.files.iter().find(|f| f.0 == id).unwrap().3
        }

        fn edit(&mut self, id: u64, cmd: &str) -> Result<(), String> {
            self.started.clear();
            run(self, Some(id), cmd)
        }
    }

    impl Host for Mock {
        fn files(&self) -> Vec<FileInfo> {
            self.files
                .iter()
                .map(|f| FileInfo {
                    id: f.0,
                    name: f.1.clone(),
                    dirty: f.4,
                })
                .collect()
        }
        fn text(&self, id: u64) -> Vec<char> {
            self.body(id).chars().collect()
        }
        fn dot(&self, id: u64) -> Range {
            self.dotted(id)
        }
        fn set_dot(&mut self, id: u64, r: Range) {
            self.f(id).3 = r;
        }
        fn replace(&mut self, id: u64, q0: usize, q1: usize, s: &str) {
            if !self.started.contains(&id) {
                self.started.push(id);
                let t = self.body(id).to_string();
                self.undo.entry(id).or_default().push(t);
            }
            let f = self.f(id);
            let t: Vec<char> = f.2.chars().collect();
            let mut n: String = t[..q0].iter().collect();
            n.push_str(s);
            n.extend(&t[q1..]);
            f.2 = n;
            f.4 = true;
        }
        fn undo(&mut self, id: u64, redo: bool) -> bool {
            let from = if redo { &mut self.redo } else { &mut self.undo };
            let Some(t) = from.get_mut(&id).and_then(Vec::pop) else {
                return false;
            };
            let old = std::mem::replace(&mut self.f(id).2, t);
            let to = if redo { &mut self.undo } else { &mut self.redo };
            to.entry(id).or_default().push(old);
            true
        }
        fn set_name(&mut self, id: u64, name: &str) {
            self.f(id).1 = name.to_string();
        }
        fn set_clean(&mut self, id: u64) {
            self.f(id).4 = false;
        }
        fn dir(&self, _id: Option<u64>) -> String {
            String::from("/d")
        }
        fn open(&mut self, path: &str) -> Result<u64, String> {
            if let Some(f) = self.files.iter().find(|f| f.1 == path) {
                return Ok(f.0);
            }
            let t = self.disk.get(path).cloned().unwrap_or_default();
            let id = self.next;
            self.next += 1;
            self.files.push((id, path.to_string(), t, (0, 0), false));
            Ok(id)
        }
        fn close(&mut self, id: u64) -> Result<(), String> {
            self.files.retain(|f| f.0 != id);
            Ok(())
        }
        fn read_file(&mut self, path: &str) -> Result<String, String> {
            self.disk
                .get(path)
                .cloned()
                .ok_or_else(|| format!("{path}: file does not exist"))
        }
        fn write_file(&mut self, path: &str, s: &str) -> Result<(), String> {
            self.disk.insert(path.to_string(), s.to_string());
            Ok(())
        }
        fn run(&mut self, _dir: &str, cmd: &str, input: Option<&str>) -> String {
            // Two commands: `up` upper-cases its input, `echo` echoes.
            match cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b)) {
                ("up", _) => input.unwrap_or("").to_uppercase(),
                ("echo", rest) => format!("{rest}\n"),
                _ => format!("{cmd}: not found\n"),
            }
        }
        fn print(&mut self, s: &str) {
            self.out.push_str(s);
        }
    }

    const ABC: &str = "one two\nthree four\nfive\n";

    /// Runs `cmd` on `text` with dot `dot`; gives the body and dot after.
    fn ed(text: &str, dot: Range, cmd: &str) -> (String, Range) {
        let mut m = Mock::new(&[("/d/f", text)]);
        m.set_dot(1, dot);
        if let Err(e) = m.edit(1, cmd) {
            return (format!("error: {e}"), (0, 0));
        }
        (m.body(1).to_string(), m.dotted(1))
    }

    #[test]
    fn table() {
        let t = |s: &str| s.to_string();
        let cases: &[(&str, Range, &str, String, Range)] = &[
            // Addresses alone set dot.
            (ABC, (0, 0), "2", t(ABC), (8, 19)),
            (ABC, (0, 0), "/four/", t(ABC), (14, 18)),
            (ABC, (0, 0), "#3,#5", t(ABC), (3, 5)),
            (ABC, (0, 0), "$", t(ABC), (24, 24)),
            (ABC, (8, 19), ".+1", t(ABC), (19, 24)),
            (ABC, (0, 0), "/t/;/e/", t(ABC), (4, 12)),
            // Text commands, and dot after them.
            ("abc", (1, 2), "a/X/", t("abXc"), (2, 3)),
            ("abc", (1, 2), "i/X/", t("aXbc"), (1, 2)),
            ("abc", (1, 2), "c/XY/", t("aXYc"), (1, 3)),
            ("abc", (1, 2), "d", t("ac"), (1, 1)),
            ("abc", (0, 0), "a/x\\ny/", t("x\nyabc"), (0, 3)),
            ("abc", (0, 0), "$a\nl1\nl2\n.\n", t("abcl1\nl2\n"), (3, 9)),
            ("abc", (0, 0), ",c/a\\/b/", t("a/b"), (0, 3)),
            (ABC, (0, 0), "2d", t("one two\nfive\n"), (8, 8)),
            (ABC, (0, 0), "1,2c/x\\n/", t("x\nfive\n"), (0, 2)),
            // Substitution: first, Nth, all, groups, & and escapes.
            ("aaa", (0, 3), "s/a/b/", t("baa"), (0, 3)),
            ("aaa", (0, 3), "s2/a/b/", t("aba"), (0, 3)),
            ("aaa", (0, 3), "s/a/b/g", t("bbb"), (0, 3)),
            ("aaa", (0, 0), ",s/a/bb/g", t("bbbbbb"), (0, 6)),
            (
                "ab cd",
                (0, 5),
                "s/([a-z])([a-z])/\\2\\1/g",
                t("ba dc"),
                (0, 5),
            ),
            ("ab", (0, 2), "s/b/[&]/", t("a[b]"), (0, 4)),
            ("ab", (0, 2), "s/b/\\&\\n/", t("a&\n"), (0, 3)),
            ("a.b", (0, 3), "s/\\./-/", t("a-b"), (0, 3)),
            ("ab", (0, 2), "s/z/y/", t("error: no substitution"), (0, 0)),
            (
                "x1 x22",
                (0, 0),
                ",s/x([0-9]+)/<\\1>/g",
                t("<1> <22>"),
                (0, 8),
            ),
            (
                "one\ntwo\n",
                (0, 0),
                ",s/$/;/g",
                t("one;\ntwo;\n;"),
                (0, 11),
            ),
            // Move and copy.
            ("abcdef", (0, 2), "m$", t("cdefab"), (4, 6)),
            ("abcdef", (4, 6), "m0", t("efabcd"), (0, 2)),
            ("abcdef", (0, 2), "t$", t("abcdefab"), (6, 8)),
            (
                "abcdef",
                (0, 4),
                "m#2",
                t("error: move overlaps itself"),
                (0, 0),
            ),
            ("l1\nl2\nl3\n", (0, 0), "1m3", t("l2\nl3\nl1\n"), (6, 9)),
            // Loops.
            (
                "a b c",
                (0, 0),
                ",x/[a-z]/s/.+/<&>/",
                t("<a> <b> <c>"),
                (8, 11),
            ),
            ("a b c", (0, 0), ",x/[a-z]/i/-/", t("-a -b -c"), (6, 7)),
            ("a,b,,c", (0, 0), ",y/,/c/X/", t("X,X,X,X"), (6, 7)),
            ("l1\nl2\n", (0, 0), ",x a/!/", t("l1\n!l2\n!"), (7, 8)),
            ("ab\ncd\nab\n", (0, 0), ",x g/a/d", t("cd\n"), (3, 3)),
            ("ab\ncd\nab\n", (0, 0), ",x v/a/d", t("ab\nab\n"), (3, 3)),
            (
                "ab\ncd\n",
                (0, 0),
                ",x/.*\\n/ s/(.*)/[\\1]/",
                t("[ab]\n[cd]\n"),
                (5, 10),
            ),
            ("aaa", (0, 0), ",x/a*/c/b/", t("b"), (0, 1)),
            ("baac", (0, 0), ",x/a*/c/-/", t("-b-c-"), (4, 5)),
            (
                "one two",
                (0, 0),
                ",x/[a-z]+/ x/o/ c/0/",
                t("0ne tw0"),
                (6, 7),
            ),
            ("x", (0, 1), "g/y/d", t("x"), (0, 1)),
            ("x", (0, 1), "v/y/d", t(""), (0, 0)),
            // Groups: the same dot for each command.
            ("abc", (1, 2), "{i/</ a/>/}", t("a<b>c"), (3, 4)),
            ("abc", (1, 2), "{\ni/</\na/>/\n}", t("a<b>c"), (3, 4)),
            ("ab ab", (0, 0), ",x/ab/{i/(/ a/)/}", t("(ab) (ab)"), (8, 9)),
            ("abc", (0, 3), "{d a/x/}", t("x"), (0, 1)),
            (
                "abc",
                (0, 3),
                "{c/1/ c/2/}",
                t("error: changes not in sequence"),
                (0, 0),
            ),
            // Commands follow one another, each seeing the last's text.
            ("abc", (0, 0), ",s/a/b/g\n,s/b/c/g", t("ccc"), (0, 3)),
            // The empty regexp is the last one.
            ("abab", (0, 0), "/b/\n,s//X/g", t("aXaX"), (0, 4)),
            // Errors.
            ("abc", (0, 0), "Z", t("error: unknown command `Z'"), (0, 0)),
            (
                "abc",
                (0, 0),
                "5d",
                t("error: address out of range"),
                (0, 0),
            ),
            (
                "abc",
                (0, 0),
                "{d",
                t("error: right brace expected"),
                (0, 0),
            ),
            ("abc", (0, 0), "s a b ", t("error: bad delimiter"), (0, 0)),
            (
                "abc",
                (0, 0),
                "g d",
                t("error: no regular expression"),
                (0, 0),
            ),
            ("abc", (0, 0), ",s/(/x/", t("error: bad regexp"), (0, 0)),
            // The pipes through the host's commands.
            ("abc", (0, 3), "|up", t("ABC"), (0, 3)),
            ("ab\ncd\n", (0, 0), ",x/[a-z]+/|up", t("AB\nCD\n"), (3, 5)),
            ("abc", (1, 1), "<echo hi", t("ahi\nbc"), (1, 4)),
        ];
        for (text, dot, cmd, want, wdot) in cases {
            let got = ed(text, *dot, cmd);
            if want.starts_with("error:") {
                assert!(
                    got.0.starts_with(want.as_str()),
                    "{cmd:?} on {text:?}: {got:?}"
                );
                continue;
            }
            assert_eq!(got, (want.clone(), *wdot), "{cmd:?} on {text:?}");
        }
    }

    #[test]
    fn printing() {
        let mut m = Mock::new(&[("/d/f", ABC)]);
        m.edit(1, "2p").unwrap();
        m.edit(1, "2=").unwrap();
        m.edit(1, "1,2=").unwrap();
        m.edit(1, "/four/=#").unwrap();
        m.edit(1, "$=").unwrap();
        m.edit(1, "1>up").unwrap();
        m.edit(1, "!echo x").unwrap();
        assert_eq!(
            m.out,
            "three four\n/d/f:2\n/d/f:1,2\n/d/f:#14,#18\n/d/f:4\nONE TWO\nx\n"
        );
        assert_eq!(m.body(1), ABC);
        let _ = m.edit(1, "/nothing/");
        assert!(m.out.ends_with("Edit: address out of range\n"), "{}", m.out);
    }

    #[test]
    fn files_and_loops() {
        let mut m = Mock::new(&[
            ("/d/a.c", "int a;\n"),
            ("/d/b.h", "int b;\n"),
            ("/d/c.c", "x\n"),
        ]);
        // X runs a command in each file whose menu line matches.
        m.edit(1, "X/\\.c$/ ,s/^/\\/\\/ /").unwrap();
        assert_eq!(m.body(1), "// int a;\n");
        assert_eq!(m.body(2), "int b;\n");
        assert_eq!(m.body(3), "// x\n");
        // Y those that do not; f prints the menu line ('  modified).
        m.edit(1, "Y/\\.c$/ f").unwrap();
        assert_eq!(m.out, " +. /d/b.h\n");
        m.out.clear();
        m.edit(1, "X/'/ =").unwrap();
        assert_eq!(m.out, "/d/a.c:1\n/d/c.c:1\n");
        m.out.clear();
        // "re" picks the file for the address.
        m.edit(1, "\"b\\.h\",d").unwrap();
        assert_eq!(m.body(2), "");
        let e = m.edit(1, "\"\\.c\",d").unwrap_err();
        assert_eq!(e, EMANY);
        // b changes the current file; D deletes windows.
        m.edit(1, "b /d/c.c\n,c/C/").unwrap();
        assert_eq!(m.body(3), "C");
        m.edit(1, "D b.h").unwrap();
        assert_eq!(m.files.len(), 2);
        // B opens files and makes the last current.
        m.disk.insert("/d/new".into(), "fresh\n".into());
        m.edit(1, "B new\n,s/fresh/old/").unwrap();
        let id = m.files.iter().find(|f| f.1 == "/d/new").unwrap().0;
        assert_eq!(m.body(id), "old\n");
        // w writes, e reads, r reads into dot, f renames.
        m.edit(1, "w /d/out").unwrap();
        assert_eq!(m.disk["/d/out"], "// int a;\n");
        m.edit(1, "2r /d/out").unwrap();
        assert_eq!(m.body(1), "// int a;\n// int a;\n");
        m.edit(1, "1w /d/one").unwrap();
        assert_eq!(m.disk["/d/one"], "// int a;\n");
        m.edit(1, "e /d/new").unwrap();
        assert_eq!(m.body(1), "fresh\n");
        assert!(!m.f(1).4, "e leaves the file clean");
        assert_eq!(m.f(1).1, "/d/new");
        m.edit(1, "f /d/renamed").unwrap();
        assert_eq!(m.f(1).1, "/d/renamed");
    }

    #[test]
    fn undo_and_marks() {
        let mut m = Mock::new(&[("/d/f", "abc\n")]);
        // Several commands in one Edit are one change to undo.
        m.edit(1, ",s/a/x/\n,s/b/y/").unwrap();
        assert_eq!(m.body(1), "xyc\n");
        m.edit(1, "u").unwrap();
        assert_eq!(m.body(1), "abc\n");
        m.edit(1, "u-1").unwrap();
        assert_eq!(m.body(1), "xyc\n");
        // k sets the mark, ' is it.
        m.edit(1, "#1k\n#3\n',.c/Q/").unwrap();
        assert_eq!(m.body(1), "xQ\n");
        m.edit(1, "q").unwrap();
    }

    #[test]
    fn parses() {
        let c = parse(",x/a/ {\n d\n}").unwrap();
        assert_eq!(c.len(), 1);
        assert!(matches!(&c[0].kind, Kind::Loop('x', Some(r), _) if r == "a"));
        let c = parse("a/x/ i/y/").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(parse("u-3").unwrap()[0].kind, Kind::Undo(-3));
        assert_eq!(
            parse("x").unwrap()[0].kind,
            Kind::Loop(
                'x',
                None,
                Box::new(Cmd {
                    addr: None,
                    kind: Kind::Print
                })
            )
        );
        assert!(parse("}").is_err());
    }
}
