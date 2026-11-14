//! The macro processor: input stack, push-back, macro table, argument
//! collection, quoting, comments, diversions and the built-ins. All I/O
//! goes through [`Host`], so the processor runs in unit tests.

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use purgatorio::bufio::Iobuf;

use crate::eval::{EvalError, eval};

/// What the processor needs from the system.
pub trait Host {
    /// Writes processed text to the standard output (buffered).
    fn out(&mut self, s: &str);
    /// Flushes the standard output.
    fn flush(&mut self);
    /// Writes to the diagnostic output, unbuffered.
    fn err(&mut self, s: &str);
    /// Opens a file for reading; the error is the system's error string.
    fn open(&mut self, path: &str) -> Result<Box<dyn Read>, String>;
    /// Runs `cmd` with sh and waits for it. The error is a load failure.
    fn syscmd(&mut self, cmd: &str) -> Result<(), String>;
    /// Whether `path` exists.
    fn exists(&mut self, path: &str) -> bool;
    /// The contents of `/dev/sysname`, if it can be read.
    fn sysname(&mut self) -> Option<String>;
    /// This prog's pid.
    fn pid(&mut self) -> u32;
}

/// A fatal error, already formatted as `m4: [file:line:] msg`.
#[derive(Debug, PartialEq, Eq)]
pub struct Fatal(pub String);

type Res<T = ()> = Result<T, Fatal>;

const NHASH: usize = 131;
const NDIV: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Builtin {
    Changecom,
    Changequote,
    Copydef,
    Define,
    Divert,
    Divnum,
    Dnl,
    Dumpdef,
    Errprint,
    Eval,
    Ifdef,
    Ifelse,
    Include,
    Incr,
    Index,
    Len,
    Maketemp,
    Sinclude,
    Substr,
    Syscmd,
    Translit,
    Undefine,
    Undivert,
}

const BUILTINS: [(&str, Builtin); 23] = [
    ("changecom", Builtin::Changecom),
    ("changequote", Builtin::Changequote),
    ("copydef", Builtin::Copydef),
    ("define", Builtin::Define),
    ("divert", Builtin::Divert),
    ("divnum", Builtin::Divnum),
    ("dnl", Builtin::Dnl),
    ("dumpdef", Builtin::Dumpdef),
    ("errprint", Builtin::Errprint),
    ("eval", Builtin::Eval),
    ("ifdef", Builtin::Ifdef),
    ("ifelse", Builtin::Ifelse),
    ("include", Builtin::Include),
    ("incr", Builtin::Incr),
    ("index", Builtin::Index),
    ("len", Builtin::Len),
    ("maketemp", Builtin::Maketemp),
    ("sinclude", Builtin::Sinclude),
    ("substr", Builtin::Substr),
    ("syscmd", Builtin::Syscmd),
    ("translit", Builtin::Translit),
    ("undefine", Builtin::Undefine),
    ("undivert", Builtin::Undivert),
];

#[derive(Clone, Debug)]
enum Body {
    Builtin(Builtin),
    /// Replacement text, and whether it is never rescanned (`-Q`).
    Text(Rc<str>, bool),
}

#[derive(Clone, Debug)]
struct Name {
    name: String,
    body: Body,
}

/// A macro table entry. A call holds on to its entry while it collects
/// arguments, so a redefinition made meanwhile is the one expanded.
type Entry = Rc<RefCell<Name>>;

struct File {
    id: u64,
    name: String,
    line: usize,
    r: Iobuf<Box<dyn Read>>,
}

pub struct M4<H: Host> {
    pub host: H,
    names: Vec<Vec<Entry>>,
    pushback: Vec<char>,
    diverted: [String; NDIV],
    curdiv: i32,
    curarg: Option<String>,
    files: Vec<File>,
    /// Where finished files ended, by id, for errors at end of input.
    ended: Vec<(u64, String, usize)>,
    next_id: u64,
    lquote: char,
    rquote: char,
    initcom: Vec<char>,
    endcom: Vec<char>,
    prefix: String,
    prefix0: Option<char>,
    tracing: bool,
    sysname: Option<String>,
}

fn isspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// Limbo's `int s`: optional blanks and sign, then decimal digits.
pub fn atoi(s: &str) -> i32 {
    let s = s.trim_start_matches(isspace);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut n: i64 = 0;
    for b in s.bytes().take_while(u8::is_ascii_digit) {
        n = n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
    }
    (if neg { n.wrapping_neg() } else { n }) as i32
}

/// Inferno's `%q`: quoted with `'` only when the string is empty or holds
/// a blank, control character or quote.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && !s.chars().any(|c| c <= ' ' || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// The bucket of `name` in a table of [`NHASH`] lists.
fn hash(name: &str) -> usize {
    let mut h: i32 = 0;
    for c in name.chars() {
        h = h.wrapping_mul(65599).wrapping_add(c as i32);
    }
    (h & i32::MAX) as usize % NHASH
}

impl<H: Host> M4<H> {
    /// A processor with `inferno` defined and the built-ins installed,
    /// their names prefixed by `prefix`.
    pub fn new(host: H, prefix: &str, tracing: bool) -> M4<H> {
        let mut chars = prefix.chars();
        let prefix0 = match (chars.next(), chars.next()) {
            (Some(c), None) if c > '\u{a0}' => Some(c),
            _ => None,
        };
        let mut m = M4 {
            host,
            names: vec![Vec::new(); NHASH],
            pushback: Vec::new(),
            diverted: Default::default(),
            curdiv: 0,
            curarg: None,
            files: Vec::new(),
            ended: Vec::new(),
            next_id: 0,
            lquote: '`',
            rquote: '\'',
            initcom: vec!['#'],
            endcom: vec!['\n'],
            prefix: String::from(prefix),
            prefix0,
            tracing,
            sysname: None,
        };
        m.define("inferno", "inferno", false);
        for (name, b) in BUILTINS {
            let name = format!("{}{name}", m.prefix);
            m.ibuiltin(&name, b);
        }
        m
    }

    // The macro table.

    fn lookup(&self, name: &str) -> Option<Entry> {
        self.names[hash(name)]
            .iter()
            .find(|n| n.borrow().name == name)
            .cloned()
    }

    fn ibuiltin(&mut self, name: &str, b: Builtin) {
        let n = Name {
            name: String::from(name),
            body: Body::Builtin(b),
        };
        self.names[hash(name)].insert(0, Rc::new(RefCell::new(n)));
    }

    fn define(&mut self, name: &str, repl: &str, asis: bool) {
        let n = Name {
            name: String::from(name),
            body: Body::Text(Rc::from(repl), asis),
        };
        match self.lookup(name) {
            Some(e) => *e.borrow_mut() = n,
            None => self.names[hash(name)].insert(0, Rc::new(RefCell::new(n))),
        }
    }

    pub fn undefine(&mut self, name: &str) {
        let b = &mut self.names[hash(name)];
        if let Some(i) = b.iter().position(|n| n.borrow().name == name) {
            b.remove(i);
        }
    }

    /// `-D` and `-Q`: `name=value`, which may not replace a built-in.
    pub fn argdefine(&mut self, s: &str, asis: bool) -> Res {
        let (name, text) = s.split_once('=').unwrap_or((s, ""));
        if let Some(n) = self.lookup(name)
            && matches!(n.borrow().body, Body::Builtin(_))
        {
            return Err(self.error(&format!("can't redefine built-in {name}")));
        }
        self.define(name, text, asis);
        Ok(())
    }

    // Errors.

    fn fatal(&mut self, loc: Option<(String, usize)>, msg: &str) -> Fatal {
        self.host.flush();
        match loc {
            Some((name, line)) => Fatal(format!("m4: {name}:{line}: {msg}")),
            None => Fatal(format!("m4: {msg}")),
        }
    }

    pub fn error(&mut self, msg: &str) -> Fatal {
        let loc = self.files.last().map(|f| (f.name.clone(), f.line));
        self.fatal(loc, msg)
    }

    /// The input file being read now, to report an error at end of input
    /// against it, as it stands then.
    fn mark(&self) -> Option<u64> {
        self.files.last().map(|f| f.id)
    }

    fn error_at(&mut self, mark: Option<u64>, msg: &str) -> Fatal {
        let loc = mark.and_then(|id| {
            self.files
                .iter()
                .find(|f| f.id == id)
                .map(|f| (f.name.clone(), f.line))
                .or_else(|| {
                    self.ended
                        .iter()
                        .rev()
                        .find(|e| e.0 == id)
                        .map(|e| (e.1.clone(), e.2))
                })
        });
        self.fatal(loc, msg)
    }

    // Input.

    pub fn pushfile(&mut self, name: &str, r: Box<dyn Read>) {
        self.next_id += 1;
        self.files.push(File {
            id: self.next_id,
            name: String::from(name),
            line: 1,
            r: Iobuf::new(r),
        });
    }

    fn getc(&mut self) -> Option<char> {
        if let Some(c) = self.pushback.pop() {
            return Some(c);
        }
        while let Some(f) = self.files.last_mut() {
            // A read error ends the file, as end of file does.
            if let Ok(Some(c)) = f.r.getc() {
                if c == '\n' {
                    f.line += 1;
                }
                return Some(c);
            }
            if let Some(f) = self.files.pop() {
                self.ended.push((f.id, f.name, f.line));
            }
        }
        None
    }

    fn pushc(&mut self, c: Option<char>) {
        if let Some(c) = c {
            self.pushback.push(c);
        }
    }

    fn pushs(&mut self, s: &str) {
        self.pushback.extend(s.chars().rev());
    }

    fn skipws(&mut self) {
        let mut c = self.getc();
        while c.is_some_and(isspace) {
            c = self.getc();
        }
        self.pushc(c);
    }

    fn isprefix(&self, c: char) -> bool {
        self.prefix0 == Some(c)
    }

    fn isalpha(&self, c: char) -> bool {
        c.is_ascii_alphabetic()
            || c == '_'
            || (c > '\u{a0}' && c != self.lquote && c != self.rquote && !self.isprefix(c))
    }

    // Output.

    fn puts(&mut self, s: &str) {
        if let Some(a) = &mut self.curarg {
            a.push_str(s);
        } else if self.curdiv > 0 {
            self.diverted[self.curdiv as usize].push_str(s);
        } else if self.curdiv == 0 {
            self.host.out(s);
        }
    }

    fn putc(&mut self, c: char) {
        let mut b = [0u8; 4];
        self.puts(c.encode_utf8(&mut b));
    }

    // Scanning.

    /// Processes input until every file and all pushed-back text is read.
    pub fn scan(&mut self) -> Res {
        while let Some(c) = self.getc() {
            if self.isalpha(c) || self.isprefix(c) {
                self.called(c)?;
            } else if c == self.lquote {
                self.quoted()?;
            } else if self.initcom.first() == Some(&c) {
                self.comment()?;
            } else {
                self.putc(c);
            }
        }
        Ok(())
    }

    /// Writes out the diversions, in order, at the end of input.
    pub fn finish(&mut self) {
        for i in 1..NDIV {
            let s = std::mem::take(&mut self.diverted[i]);
            self.host.out(&s);
        }
        self.host.flush();
    }

    fn called(&mut self, first: char) -> Res {
        let mut tok = String::from(first);
        let mut c = self.getc();
        while let Some(ch) = c {
            if !(self.isalpha(ch) || ch.is_ascii_digit()) {
                break;
            }
            tok.push(ch);
            c = self.getc();
        }
        let Some(def) = self.lookup(&tok) else {
            self.pushc(c);
            self.puts(&tok);
            return Ok(());
        };
        let asis = matches!(def.borrow().body, Body::Text(_, true));
        if c != Some('(') || asis {
            self.pushc(c);
            return self.expand(&def, vec![tok]);
        }
        let mut args = vec![def.borrow().name.clone()];
        let saved = self.curarg.replace(String::new());
        let mut nesting = 0;
        self.skipws();
        let mark = self.mark();
        loop {
            let Some(c) = self.getc() else {
                return Err(self.error_at(mark, "EOF in parameters"));
            };
            if self.isalpha(c) || self.isprefix(c) {
                self.called(c)?;
            } else if c == self.lquote {
                self.quoted()?;
            } else {
                if c == '(' {
                    nesting += 1;
                }
                if nesting > 0 {
                    if c == ')' {
                        nesting -= 1;
                    }
                    self.putc(c);
                } else if c == ',' {
                    args.push(self.curarg.replace(String::new()).unwrap_or_default());
                    self.skipws();
                } else if c == ')' {
                    break;
                } else {
                    self.putc(c);
                }
            }
        }
        args.push(std::mem::replace(&mut self.curarg, saved).unwrap_or_default());
        self.expand(&def, args)
    }

    fn quoted(&mut self) -> Res {
        let mut nesting = 0;
        let mark = self.mark();
        loop {
            let Some(c) = self.getc() else {
                return Err(self.error_at(mark, "EOF in string"));
            };
            if c == self.rquote {
                if nesting == 0 {
                    return Ok(());
                }
                nesting -= 1;
            } else if c == self.lquote {
                nesting += 1;
            }
            self.putc(c);
        }
    }

    fn comment(&mut self) -> Res {
        let initcom = self.initcom.clone();
        let endcom = self.endcom.clone();
        for i in 1..initcom.len() {
            let c = self.getc();
            if c != Some(initcom[i]) {
                if c.is_none() {
                    return Err(self.error("EOF in comment"));
                }
                self.pushc(c);
                self.pushs(&initcom[1..i].iter().collect::<String>());
                self.putc(initcom[0]);
                return Ok(());
            }
        }
        self.puts(&initcom.iter().collect::<String>());
        let mut i = 0;
        while i < endcom.len() {
            let Some(c) = self.getc() else {
                return Err(self.error("EOF in comment"));
            };
            self.putc(c);
            if c == endcom[i] {
                i += 1;
            } else {
                i = usize::from(c == endcom[0]);
            }
        }
        Ok(())
    }

    fn expand(&mut self, def: &Entry, args: Vec<String>) -> Res {
        let n = def.borrow().clone();
        if self.tracing {
            let mut t = format!("expand {} [{}]", args[0], n.name);
            for (i, a) in args.iter().enumerate().skip(1) {
                t.push_str(&format!(" {i}: [{a}]"));
            }
            t.push('\n');
            self.host.err(&t);
        }
        let repl = match n.body {
            Body::Builtin(b) => return self.builtin(b, &args),
            Body::Text(repl, asis) => {
                if asis {
                    self.puts(&repl);
                    return Ok(());
                }
                repl
            }
        };
        if *repl == *n.name || &*repl == "$0" {
            self.puts(&n.name);
            return Ok(());
        }
        let mut out = String::new();
        let mut it = repl.chars().peekable();
        while let Some(c) = it.next() {
            if c == '$'
                && let Some(d) = it.next_if(char::is_ascii_digit)
            {
                if let Some(a) = args.get(d as usize - '0' as usize) {
                    out.push_str(a);
                }
            } else {
                out.push(c);
            }
        }
        self.pushs(&out);
        Ok(())
    }

    // The built-ins. `a[0]` is the macro's name.

    fn builtin(&mut self, b: Builtin, a: &[String]) -> Res {
        let n = a.len();
        match b {
            Builtin::Changecom => match n {
                1 => {
                    self.initcom.clear();
                    self.endcom.clear();
                }
                2 => {
                    self.initcom = a[1].chars().collect();
                    self.endcom = vec!['\n'];
                }
                _ => {
                    self.initcom = a[1].chars().collect();
                    self.endcom = a[2].chars().collect();
                    if self.endcom.is_empty() {
                        self.endcom = vec!['\n'];
                    }
                }
            },
            Builtin::Changequote => match n {
                1 => {
                    self.lquote = '`';
                    self.rquote = '\'';
                }
                2 => {
                    if let Some(c) = a[1].chars().next() {
                        self.lquote = c;
                        self.rquote = c;
                    }
                }
                _ => {
                    if let Some(c) = a[1].chars().next() {
                        self.lquote = c;
                    }
                    if let Some(c) = a[2].chars().next() {
                        self.rquote = c;
                    }
                }
            },
            Builtin::Copydef => {
                if n > 2 && a[1] != a[2] {
                    self.undefine(&a[2]);
                    match self.lookup(&a[1]).map(|e| e.borrow().body.clone()) {
                        Some(Body::Builtin(b)) => self.ibuiltin(&a[2], b),
                        Some(Body::Text(t, asis)) => self.define(&a[2], &t, asis),
                        None => self.define(&a[2], "", false),
                    }
                }
            }
            Builtin::Define => {
                if n > 1 {
                    self.define(&a[1], a.get(2).map_or("", |s| s), false);
                }
            }
            Builtin::Divert => {
                self.curdiv = match a.get(1) {
                    Some(s) => match atoi(s) {
                        d @ 0..=9 => d,
                        _ => -1,
                    },
                    None => 0,
                };
            }
            Builtin::Divnum => self.pushs(&self.curdiv.to_string()),
            Builtin::Dnl => while self.getc().is_some_and(|c| c != '\n') {},
            Builtin::Dumpdef => {
                let mut out = String::new();
                if n > 1 {
                    for name in &a[1..] {
                        if let Some(e) = self.lookup(name) {
                            let e = e.borrow();
                            out.push_str(&format!("{}\t{}\n", quote(&e.name), self.text(&e)));
                        }
                    }
                } else {
                    for bucket in &self.names {
                        for e in bucket {
                            let e = e.borrow();
                            out.push_str(&format!("{} {}\n", quote(&e.name), self.text(&e)));
                        }
                    }
                }
                self.host.err(&out);
            }
            Builtin::Errprint => {
                if n > 1 {
                    let mut s = String::from("m4:");
                    for x in &a[1..] {
                        s.push(' ');
                        s.push_str(x);
                    }
                    s.push('\n');
                    self.host.err(&s);
                }
            }
            Builtin::Eval => {
                if n > 1 {
                    let v = match eval(&a[1]) {
                        Ok(v) => v,
                        Err(EvalError::Syntax(done, rest)) => {
                            let msg = format!("syntax error: {} {}", quote(&done), quote(&rest));
                            return Err(self.error(&msg));
                        }
                        Err(EvalError::Bad) => {
                            return Err(self.error(&format!("zero divide in {}", quote(&a[1]))));
                        }
                    };
                    self.pushs(&v.to_string());
                }
            }
            Builtin::Ifdef => {
                if n >= 3 {
                    if self.lookup(&a[1]).is_some() {
                        self.pushs(&a[2]);
                    } else if n > 3 {
                        self.pushs(&a[3]);
                    }
                }
            }
            Builtin::Ifelse => {
                let mut i = 1;
                while i + 2 < n {
                    if a[i] == a[i + 1] {
                        self.pushs(&a[i + 2]);
                        return Ok(());
                    }
                    i += 3;
                }
                if i > 2 && i == n - 1 {
                    self.pushs(&a[i]);
                }
            }
            Builtin::Include | Builtin::Sinclude => {
                for name in a[1..].iter().rev() {
                    match self.host.open(name) {
                        Ok(r) => self.pushfile(name, r),
                        Err(e) if b == Builtin::Include => {
                            return Err(self.error(&format!("can't open {name}: {e}")));
                        }
                        Err(_) => {}
                    }
                }
            }
            Builtin::Incr => {
                if n > 1 {
                    self.pushs(&atoi(&a[1]).wrapping_add(1).to_string());
                }
            }
            Builtin::Index => {
                if n > 2 {
                    let s: Vec<char> = a[1].chars().collect();
                    let t: Vec<char> = a[2].chars().collect();
                    let i = (0..=s.len())
                        .take_while(|i| i + t.len() <= s.len())
                        .find(|&i| s[i..i + t.len()] == t[..])
                        .map_or(-1, |i| i as i64);
                    self.pushs(&i.to_string());
                }
            }
            Builtin::Len => {
                if n > 1 {
                    self.puts(&a[1].chars().count().to_string());
                }
            }
            Builtin::Maketemp => {
                if n > 1 {
                    let t = self.maketemp(&a[1])?;
                    self.pushs(&t);
                }
            }
            Builtin::Substr => {
                if n > 2 {
                    let s: Vec<char> = a[1].chars().collect();
                    let l = s.len() as i64;
                    let o = i64::from(atoi(&a[2])).clamp(0, l);
                    let mut e = a.get(3).map_or(l, |x| i64::from(atoi(x)).clamp(0, l));
                    e = (e + o).min(l);
                    let sub: String = s[o as usize..e as usize].iter().collect();
                    self.pushs(&sub);
                }
            }
            Builtin::Syscmd => {
                if n > 1 {
                    self.host.flush();
                    if let Err(e) = self.host.syscmd(&a[1]) {
                        return Err(self.error(&e));
                    }
                }
            }
            Builtin::Translit => {
                if n >= 3 {
                    let from: Vec<char> = a[2].chars().collect();
                    let to: Vec<char> = a.get(3).map_or(Vec::new(), |t| t.chars().collect());
                    let mut o = String::new();
                    for c in a[1].chars() {
                        match from.iter().position(|&f| f == c) {
                            Some(j) => o.extend(to.get(j)),
                            None => o.push(c),
                        }
                    }
                    self.pushs(&o);
                }
            }
            Builtin::Undefine => {
                for name in &a[1..] {
                    self.undefine(name);
                }
            }
            Builtin::Undivert => {
                if n <= 1 {
                    for i in 1..NDIV {
                        if i as i32 != self.curdiv {
                            let s = std::mem::take(&mut self.diverted[i]);
                            self.puts(&s);
                        }
                    }
                } else {
                    for x in &a[1..] {
                        let d = atoi(x);
                        if d > 0 && (d as usize) < NDIV && d != self.curdiv {
                            let s = std::mem::take(&mut self.diverted[d as usize]);
                            self.puts(&s);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A definition as `dumpdef` shows it.
    fn text(&self, n: &Name) -> String {
        match &n.body {
            Body::Builtin(_) => format!("builtin {}", quote(&n.name)),
            Body::Text(t, _) => format!("{}{t}{}", self.lquote, self.rquote),
        }
    }

    /// `maketemp`: the trailing `X`s of `s` become `.sysname.pid` and a
    /// letter that makes the name unused.
    fn maketemp(&mut self, s: &str) -> Res<String> {
        if self.sysname.is_none() {
            let n = self.host.sysname().filter(|s| !s.is_empty());
            self.sysname = Some(n.unwrap_or_else(|| String::from("m4")));
        }
        let mut t = String::from(s.trim_end_matches('X'));
        if !t.is_empty() {
            t.push('.');
        }
        let pid = self.host.pid();
        t.push_str(&format!(
            "{}.{pid:010}a",
            self.sysname.as_deref().unwrap_or("m4")
        ));
        while self.host.exists(&t) {
            let last = t.pop().unwrap_or('a');
            if last == 'z' {
                t.push(last);
                return Err(self.error(&format!("out of temp files: {t}")));
            }
            t.push(char::from(last as u8 + 1));
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Test {
        out: String,
        err: String,
        files: HashMap<String, String>,
        cmds: Vec<String>,
    }

    impl Host for Test {
        fn out(&mut self, s: &str) {
            self.out.push_str(s);
        }
        fn flush(&mut self) {}
        fn err(&mut self, s: &str) {
            self.err.push_str(s);
        }
        fn open(&mut self, path: &str) -> Result<Box<dyn Read>, String> {
            match self.files.get(path) {
                Some(s) => Ok(Box::new(std::io::Cursor::new(s.clone().into_bytes()))),
                None => Err(String::from("file does not exist")),
            }
        }
        fn syscmd(&mut self, cmd: &str) -> Result<(), String> {
            self.cmds.push(String::from(cmd));
            self.out.push_str("[ran]");
            Ok(())
        }
        fn exists(&mut self, path: &str) -> bool {
            self.files.contains_key(path)
        }
        fn sysname(&mut self) -> Option<String> {
            Some(String::from("host"))
        }
        fn pid(&mut self) -> u32 {
            42
        }
    }

    fn run_with(t: Test, input: &str) -> (Result<(), Fatal>, Test) {
        let mut m = M4::new(t, "", false);
        m.pushfile(
            "standard input",
            Box::new(std::io::Cursor::new(input.to_owned())),
        );
        let r = m.scan();
        if r.is_ok() {
            m.finish();
        }
        (r, m.host)
    }

    fn run(input: &str) -> String {
        let (r, t) = run_with(Test::default(), input);
        r.unwrap();
        t.out
    }

    fn fails(input: &str) -> String {
        run_with(Test::default(), input).0.unwrap_err().0
    }

    #[test]
    fn words_and_text() {
        assert_eq!(run("hello, world\n"), "hello, world\n");
        assert_eq!(run("a1b _x 9z inferno\n"), "a1b _x 9z inferno\n");
        assert_eq!(run("define(x,y)x x1 1x (x)\n"), "y x1 1y (y)\n");
    }

    #[test]
    fn quotes_nest_and_strip_once() {
        assert_eq!(run("`a'\n"), "a\n");
        assert_eq!(run("``a' `b''\n"), "`a' `b'\n");
        assert_eq!(run("define(x,1)`x'\n"), "x\n");
        assert_eq!(run("changequote([,])[`x'] [[n]]\n"), "`x' [n]\n");
        assert_eq!(run("changequote(|)|a|changequote`b'\n"), "ab\n");
    }

    #[test]
    fn arguments() {
        let d = "define(`p',`<`$0'|$1|$2|$3>')";
        assert_eq!(run(&format!("{d}p(a,b)\n")), "<p|a|b|>\n");
        assert_eq!(run(&format!("{d}p(  a, \n b ,c )\n")), "<p|a|b |c >\n");
        assert_eq!(run(&format!("{d}p((a,b),c)\n")), "<p|(a,b)|c|>\n");
        assert_eq!(run(&format!("{d}p(`a,b',c)\n")), "<p|a,b|c|>\n");
        assert_eq!(run(&format!("{d}p\n")), "<p|||>\n");
        assert_eq!(run(&format!("{d}p ()\n")), "<p|||> ()\n");
        assert_eq!(run("define(`c',`a,b')define(`q',`[$2]')q(c)\n"), "[b]\n");
        assert_eq!(run("define(`s',`$$1$')s(x)\n"), "$x$\n");
    }

    #[test]
    fn self_reference_does_not_loop() {
        assert_eq!(run("define(`x',`x')x define(`y',`$0')y\n"), "x y\n");
    }

    #[test]
    fn recursion() {
        let fact = "define(`fact',`ifelse($1,0,1,`eval($1*fact(decr($1)))')')\
                    define(`decr',`eval($1-1)')fact(5)\n";
        assert_eq!(run(fact), "120\n");
    }

    #[test]
    fn comments() {
        assert_eq!(run("define(x,y)# x `\nx\n"), "# x `\ny\n");
        assert_eq!(
            run("changecom(/*,*/)define(x,y)/* x */x /x\n"),
            "/* x */y /y\n"
        );
        assert_eq!(run("changecom(/*,*/)/* a **/b\n"), "/* a **/b\n");
        assert_eq!(run("changecom define(x,y)# x\n"), " # y\n");
        assert_eq!(fails("# no newline"), "m4: EOF in comment");
    }

    #[test]
    fn diversions() {
        assert_eq!(
            run("divert(2)two\ndivert(1)one\ndivert`'zero\n"),
            "zero\none\ntwo\n"
        );
        assert_eq!(run("divert(1)x\ndivert(0)undivert(1)y\n"), "x\ny\n");
        assert_eq!(run("divert(-1)gone\ndivert divnum\n"), " 0\n");
        assert_eq!(
            run("divert(7)divnum`'divert(12)divnum`'divert`'divnum"),
            "07"
        );
    }

    #[test]
    fn builtins() {
        assert_eq!(run("len(abc) len(`') len(é)\n"), "3 0 1\n");
        assert_eq!(run("index(abcd,cd) index(abc,x) index(abc,)\n"), "2 -1 0\n");
        assert_eq!(
            run("substr(hello,1,3) substr(hello,3) substr(hello,9)|\n"),
            "ell lo |\n"
        );
        assert_eq!(
            run("translit(hello,lo,01) translit(hello,el)\n"),
            "he001 ho\n"
        );
        assert_eq!(run("incr(41) incr(-1) incr(x)\n"), "42 0 1\n");
        assert_eq!(
            run("ifdef(`inferno',yes,no) ifdef(`nope',yes,no) ifdef(`x',y)|\n"),
            "yes no |\n"
        );
        assert_eq!(
            run("ifelse(a,a,1) ifelse(a,b,1,2) ifelse(a,b,1,c,c,3,4) ifelse(a,b,1)|\n"),
            "1 2 3 |\n"
        );
        assert_eq!(run("define(x,1)undefine(`x')x\n"), "x\n");
        assert_eq!(run("copydef(`define',`def')def(y,2)y\n"), "2\n");
        assert_eq!(run("define(a,b)a`'dnl junk\nc\n"), "bc\n");
        assert_eq!(run("eval(2**10) eval(-7/2)\n"), "1024 -3\n");
    }

    #[test]
    fn redefinition_during_collection() {
        assert_eq!(run("define(x,`a$1')x(define(`x',`b$1'))\n"), "b\n");
    }

    #[test]
    fn include_and_syscmd() {
        let mut t = Test::default();
        t.files
            .insert(String::from("inc"), String::from("define(z,9)in\n"));
        let (r, t) = run_with(t, "include(inc)z sinclude(nope)syscmd(echo hi)\n");
        r.unwrap();
        assert_eq!(t.out, "in\n9 [ran]\n");
        assert_eq!(t.cmds, ["echo hi"]);
        assert_eq!(
            fails("x\ninclude(nope)"),
            "m4: standard input:2: can't open nope: file does not exist"
        );
    }

    #[test]
    fn maketemp_and_dumpdef() {
        assert_eq!(
            run("maketemp(/tmp/fooXXX)\n"),
            "/tmp/foo.host.0000000042a\n"
        );
        let mut t = Test::default();
        t.files
            .insert(String::from("host.0000000042a"), String::new());
        let (r, t) = run_with(t, "maketemp(XXXX) define(`d',`v w')dumpdef(`d',`len')\n");
        r.unwrap();
        assert_eq!(t.out, "host.0000000042b \n");
        assert_eq!(t.err, "d\t`v w'\nlen\tbuiltin len\n");
    }

    #[test]
    fn errors() {
        assert_eq!(
            fails("define(x,"),
            "m4: standard input:1: EOF in parameters"
        );
        assert_eq!(fails("\n`abc"), "m4: standard input:2: EOF in string");
        assert_eq!(
            fails("eval(1+)"),
            "m4: standard input:1: syntax error: 1+ ''"
        );
        assert_eq!(
            fails("eval(1/0)"),
            "m4: standard input:1: zero divide in 1/0"
        );
        let mut m = M4::new(Test::default(), "", false);
        assert_eq!(
            m.argdefine("len=3", false),
            Err(Fatal(String::from("m4: can't redefine built-in len")))
        );
    }

    #[test]
    fn prefix_and_asis() {
        let mut m = M4::new(Test::default(), "m4_", false);
        m.argdefine("q=`a' x", true).unwrap();
        m.argdefine("x=y", false).unwrap();
        m.pushfile(
            "in",
            Box::new(std::io::Cursor::new("define m4_define(a,b)a q(1)\n")),
        );
        m.scan().unwrap();
        assert_eq!(m.host.out, "define b `a' x(1)\n");
    }

    #[test]
    fn trace() {
        let mut m = M4::new(Test::default(), "", true);
        m.pushfile("in", Box::new(std::io::Cursor::new("len(ab)")));
        m.scan().unwrap();
        assert_eq!(m.host.err, "expand len [len] 1: [ab]\n");
    }

    #[test]
    fn helpers() {
        assert_eq!(atoi("  -12x"), -12);
        assert_eq!(atoi("+7"), 7);
        assert_eq!(atoi("x"), 0);
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), "'it''s'");
        assert_eq!(quote("plain"), "plain");
    }
}
