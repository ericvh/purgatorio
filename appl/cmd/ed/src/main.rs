//! ed [-] [-o] [file]: the line editor, after Plan 9's ed(1).
//!
//! Addresses: `.`, `$`, a line number, `'x` (a mark), `/re/` and `?re?`
//! (searches forward and back, wrapping), each followed by any number of
//! `+n` and `-n` (or `^n`); `a,b` and `a;b` (which sets dot to `a` first);
//! `,` alone is `1,$` and `;` alone is `.,$`. Regular expressions are
//! those of regexp(6); an empty one repeats the last.
//!
//! Commands: `a i c` (text up to a line holding `.`), `d`, `p n l`, `=`,
//! `e E f r w W file`, `g v /re/commands`, `j`, `kx`, `m t addr`, `q Q`,
//! `s/re/replacement/[gnpl]` (with `&` and `\1`..`\9`), `u` and `!cmd`.
//! `p`, `l` or `n` after most commands prints the new current line. An
//! empty command prints the next line. Errors print `?`.
//!
//! `-` suppresses the byte counts and the `!` after shell commands. `-o`
//! sends that output to standard error, so that `w` with no file name
//! can write the buffer to standard output.

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};

use purgatorio::{Context, Error};
use regexp::Regex;

/// An error: the text after `?`, usually empty.
struct Fail(String);

type R<T> = Result<T, Fail>;

fn fail<T>() -> R<T> {
    Err(Fail(String::new()))
}

/// A line of the buffer, with an identity that follows it as lines move,
/// for marks and the global command.
#[derive(Clone)]
struct Line {
    id: u64,
    text: String,
}

/// A command line being parsed.
struct Cur {
    s: Vec<char>,
    i: usize,
}

impl Cur {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if start == self.i {
            return None;
        }
        self.s[start..self.i]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    fn rest(&mut self) -> String {
        let r: String = self.s[self.i..].iter().collect();
        self.i = self.s.len();
        r
    }

    fn skip_blanks(&mut self) {
        while self.peek() == Some(' ') || self.peek() == Some('\t') {
            self.i += 1;
        }
    }

    /// Text up to an unescaped `delim` (consumed) or the end of the line,
    /// with `\delim` turned into `delim`.
    fn delimited(&mut self, delim: char) -> (String, bool) {
        let mut out = String::new();
        while let Some(c) = self.next() {
            if c == delim {
                return (out, true);
            }
            if c == '\\'
                && let Some(n) = self.peek()
            {
                self.i += 1;
                if n != delim {
                    out.push('\\');
                }
                out.push(n);
                continue;
            }
            out.push(c);
        }
        (out, false)
    }
}

struct Ed {
    buf: Vec<Line>,
    next_id: u64,
    dot: usize,
    marks: [Option<u64>; 26],
    file: Option<String>,
    dirty: bool,
    warned: bool,
    last_re: Option<String>,
    undo: Option<(Vec<Line>, usize)>,
    quiet: bool,
    /// Messages (counts, `?`, `!`) and printed lines go here.
    out: Box<dyn Write>,
    /// With `-o`, `w` and no file writes here.
    stdout_w: bool,
    input: Box<dyn BufRead>,
    /// Lines of a global command list being run, before standard input.
    queue: Vec<VecDeque<String>>,
    in_global: bool,
    done: bool,
}

impl Ed {
    fn new_line(&mut self, text: String) -> Line {
        self.next_id += 1;
        Line {
            id: self.next_id,
            text,
        }
    }

    fn dollar(&self) -> usize {
        self.buf.len()
    }

    /// The next input line, without its newline: from the running global
    /// command list, or standard input.
    fn read_line(&mut self) -> io::Result<Option<String>> {
        if let Some(q) = self.queue.last_mut() {
            return Ok(q.pop_front());
        }
        let mut b = Vec::new();
        if self.input.read_until(b'\n', &mut b)? == 0 {
            return Ok(None);
        }
        if b.last() == Some(&b'\n') {
            b.pop();
        }
        Ok(Some(String::from_utf8_lossy(&b).into_owned()))
    }

    fn save_undo(&mut self) {
        if !self.in_global {
            self.undo = Some((self.buf.clone(), self.dot));
        }
    }

    fn regex(&mut self, pat: String) -> R<Regex> {
        let pat = if pat.is_empty() {
            self.last_re.clone().ok_or(Fail(String::new()))?
        } else {
            pat
        };
        let re = Regex::new(&pat).map_err(|_| Fail(String::new()))?;
        self.last_re = Some(pat);
        Ok(re)
    }

    fn search(&mut self, c: &mut Cur, delim: char) -> R<usize> {
        let (pat, _) = c.delimited(delim);
        let re = self.regex(pat)?;
        let n = self.dollar();
        if n == 0 {
            return fail();
        }
        let mut l = self.dot;
        for _ in 0..n {
            l = if delim == '/' {
                if l >= n { 1 } else { l + 1 }
            } else if l <= 1 {
                n
            } else {
                l - 1
            };
            if re.is_match(&self.buf[l - 1].text) {
                return Ok(l);
            }
        }
        fail()
    }

    /// One address, or None when there is none.
    fn address(&mut self, c: &mut Cur) -> R<Option<usize>> {
        c.skip_blanks();
        let mut base: Option<i64> = match c.peek() {
            Some('.') => {
                c.i += 1;
                Some(self.dot as i64)
            }
            Some('$') => {
                c.i += 1;
                Some(self.dollar() as i64)
            }
            Some(d) if d.is_ascii_digit() => c.number().map(|n| n as i64),
            Some('\'') => {
                c.i += 1;
                let m = c
                    .next()
                    .filter(char::is_ascii_lowercase)
                    .ok_or(Fail(String::new()))?;
                let id = self.marks[(m as u8 - b'a') as usize].ok_or(Fail(String::new()))?;
                let l = self
                    .buf
                    .iter()
                    .position(|l| l.id == id)
                    .ok_or(Fail(String::new()))?;
                Some(l as i64 + 1)
            }
            Some(d @ ('/' | '?')) => {
                c.i += 1;
                Some(self.search(c, d)? as i64)
            }
            _ => None,
        };
        loop {
            c.skip_blanks();
            let sign = match c.peek() {
                Some('+') => 1,
                Some('-' | '^') => -1,
                Some(d) if d.is_ascii_digit() && base.is_some() => {
                    // `.5` is `.+5`.
                    let n = c.number().unwrap_or(0) as i64;
                    base = base.map(|b| b + n);
                    continue;
                }
                _ => break,
            };
            c.i += 1;
            let n = c.number().unwrap_or(1) as i64;
            base = Some(base.unwrap_or(self.dot as i64) + sign * n);
        }
        match base {
            None => Ok(None),
            Some(b) if b < 0 || b as usize > self.dollar() => fail(),
            Some(b) => Ok(Some(b as usize)),
        }
    }

    /// The address list: how many were given, and the two lines.
    fn addresses(&mut self, c: &mut Cur) -> R<(usize, usize, usize)> {
        let mut count = 0;
        let (mut a1, mut a2) = (self.dot, self.dot);
        let first = self.address(c)?;
        if let Some(a) = first {
            count = 1;
            a1 = a;
            a2 = a;
        }
        loop {
            c.skip_blanks();
            let sep = match c.peek() {
                Some(s @ (',' | ';')) => s,
                _ => break,
            };
            c.i += 1;
            if count == 0 {
                a2 = if sep == ',' { 1 } else { self.dot };
            }
            if sep == ';' {
                self.dot = a2;
            }
            a1 = a2;
            a2 = match self.address(c)? {
                Some(a) => a,
                None => self.dollar(),
            };
            count = 2;
        }
        Ok((count, a1, a2))
    }

    fn print(&mut self, from: usize, to: usize, how: char) -> R<()> {
        if from == 0 || to > self.dollar() || from > to {
            return fail();
        }
        for l in from..=to {
            let t = &self.buf[l - 1].text;
            let s = match how {
                'n' => format!("{l}\t{t}"),
                'l' => list(t),
                _ => t.clone(),
            };
            let _ = writeln!(self.out, "{s}");
        }
        self.dot = to;
        Ok(())
    }

    /// Reads text lines up to `.`.
    fn text(&mut self) -> R<Vec<String>> {
        let mut out = Vec::new();
        loop {
            match self.read_line() {
                Ok(Some(l)) if l == "." => return Ok(out),
                Ok(Some(l)) => out.push(l),
                _ => return Ok(out),
            }
        }
    }

    fn insert(&mut self, after: usize, text: Vec<String>) {
        let lines: Vec<Line> = text.into_iter().map(|t| self.new_line(t)).collect();
        let n = lines.len();
        self.buf.splice(after..after, lines);
        self.dot = after + n;
        if n > 0 {
            self.dirty = true;
        }
    }

    fn delete(&mut self, a1: usize, a2: usize) {
        self.buf.drain(a1 - 1..a2);
        self.dirty = true;
        self.dot = if a1 > self.dollar() {
            self.dollar()
        } else {
            a1
        };
    }

    fn filename(&mut self, c: &mut Cur, set: bool) -> R<String> {
        let had_blank = matches!(c.peek(), Some(' ' | '\t'));
        c.skip_blanks();
        let name = c.rest();
        if !name.is_empty() && !had_blank {
            return fail();
        }
        if name.is_empty() {
            return self.file.clone().ok_or(Fail(String::new()));
        }
        if set || self.file.is_none() {
            self.file = Some(name.clone());
        }
        Ok(name)
    }

    fn count(&mut self, n: usize) {
        if !self.quiet {
            let _ = writeln!(self.out, "{n}");
        }
    }

    fn read_file(&mut self, name: &str) -> R<Vec<String>> {
        let data = std::fs::read(name).map_err(|_| Fail(String::from(name)))?;
        self.count(data.len());
        let text = String::from_utf8_lossy(&data);
        Ok(text
            .split_inclusive('\n')
            .map(|l| String::from(l.strip_suffix('\n').unwrap_or(l)))
            .collect())
    }

    fn write_file(&mut self, name: Option<&str>, a1: usize, a2: usize, append: bool) -> R<()> {
        let mut data = String::new();
        if a1 > 0 {
            for l in &self.buf[a1 - 1..a2] {
                data.push_str(&l.text);
                data.push('\n');
            }
        }
        match name {
            None => {
                let mut so = io::stdout().lock();
                so.write_all(data.as_bytes())
                    .map_err(|_| Fail(String::new()))?;
                let _ = so.flush();
            }
            Some(name) => {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .append(append)
                    .truncate(!append)
                    .open(name)
                    .map_err(|_| Fail(String::from(name)))?;
                f.write_all(data.as_bytes())
                    .map_err(|_| Fail(String::from(name)))?;
            }
        }
        self.count(data.len());
        Ok(())
    }

    fn substitute(&mut self, c: &mut Cur, a1: usize, a2: usize) -> R<char> {
        let delim = c
            .next()
            .filter(|d| *d != ' ' && *d != '\n')
            .ok_or(Fail(String::new()))?;
        let (pat, _) = c.delimited(delim);
        let re = self.regex(pat)?;
        // The replacement, which may continue on the next lines after a
        // trailing backslash.
        let mut rep = String::new();
        loop {
            match c.next() {
                None => break,
                Some(ch) if ch == delim => break,
                Some('\\') => match c.next() {
                    Some(n) if n == delim => rep.push(n),
                    Some(n) => {
                        rep.push('\\');
                        rep.push(n);
                    }
                    None => {
                        rep.push('\n');
                        match self.read_line() {
                            Ok(Some(l)) => {
                                c.s = l.chars().collect();
                                c.i = 0;
                            }
                            _ => break,
                        }
                    }
                },
                Some(ch) => rep.push(ch),
            }
        }
        let mut global = false;
        let mut nth = 1;
        let mut suffix = ' ';
        loop {
            match c.peek() {
                Some('g') => global = true,
                Some(d) if d.is_ascii_digit() => {
                    nth = c.number().unwrap_or(1).max(1);
                    continue;
                }
                Some(p @ ('p' | 'l' | 'n')) => suffix = p,
                _ => break,
            }
            c.i += 1;
        }
        if a1 == 0 {
            return fail();
        }
        let mut last = None;
        let mut l = a1;
        let mut end = a2;
        while l <= end {
            let text = self.buf[l - 1].text.clone();
            if let Some(new) = subst_line(&re, &text, &rep, global, nth) {
                let parts: Vec<String> = new.split('\n').map(String::from).collect();
                let extra = parts.len() - 1;
                let mut lines = Vec::new();
                for (k, p) in parts.into_iter().enumerate() {
                    if k == 0 {
                        lines.push(Line {
                            id: self.buf[l - 1].id,
                            text: p,
                        });
                    } else {
                        lines.push(self.new_line(p));
                    }
                }
                self.buf.splice(l - 1..l, lines);
                l += extra;
                end += extra;
                last = Some(l);
                self.dirty = true;
            }
            l += 1;
        }
        match last {
            Some(l) => self.dot = l,
            None if self.in_global => {}
            None => return fail(),
        }
        Ok(suffix)
    }

    fn global(&mut self, c: &mut Cur, a1: usize, a2: usize, keep: bool) -> R<()> {
        if self.in_global {
            return fail();
        }
        let delim = c
            .next()
            .filter(|d| *d != ' ' && *d != '\n')
            .ok_or(Fail(String::new()))?;
        let (pat, _) = c.delimited(delim);
        let re = self.regex(pat)?;
        let mut cmds = c.rest();
        while cmds.ends_with('\\') {
            cmds.pop();
            cmds.push('\n');
            match self.read_line() {
                Ok(Some(l)) => cmds.push_str(&l),
                _ => break,
            }
        }
        if cmds.trim().is_empty() {
            cmds = String::from("p");
        }
        if a1 == 0 {
            return fail();
        }
        let ids: Vec<u64> = self.buf[a1 - 1..a2]
            .iter()
            .filter(|l| re.is_match(&l.text) == keep)
            .map(|l| l.id)
            .collect();
        self.save_undo();
        self.in_global = true;
        let mut result = Ok(());
        for id in ids {
            let Some(pos) = self.buf.iter().position(|l| l.id == id) else {
                continue;
            };
            self.dot = pos + 1;
            self.queue
                .push(cmds.split('\n').map(String::from).collect());
            while let Ok(Some(line)) = self.read_line() {
                if let Err(e) = self.command(&line) {
                    result = Err(e);
                    break;
                }
                if self.done {
                    break;
                }
            }
            self.queue.pop();
            if result.is_err() || self.done {
                break;
            }
        }
        self.in_global = false;
        result
    }

    fn shell(&mut self, cmd: &str) -> R<()> {
        let argv = vec![String::from("sh"), String::from("-c"), String::from(cmd)];
        let _ = self.out.flush();
        let pid =
            purgatorio::spawn("/bin/sh.wasm", &argv, 0, &[]).map_err(|_| Fail(String::new()))?;
        loop {
            match purgatorio::wait() {
                Ok(w) if w.pid == pid => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if !self.quiet {
            let _ = writeln!(self.out, "!");
        }
        Ok(())
    }

    /// Runs one command line.
    fn command(&mut self, line: &str) -> R<()> {
        let mut c = Cur {
            s: line.chars().collect(),
            i: 0,
        };
        let (count, a1, a2) = self.addresses(&mut c)?;
        c.skip_blanks();
        let cmd = c.next();
        // Checks a range, after defaulting it.
        let range = |ed: &Ed, a1: usize, a2: usize| -> R<()> {
            if a1 == 0 || a1 > a2 || a2 > ed.dollar() {
                return fail();
            }
            Ok(())
        };
        let mut suffix = ' ';
        match cmd {
            None => {
                let l = if count > 0 { a2 } else { self.dot + 1 };
                return self.print(l, l, 'p');
            }
            Some('a' | 'i' | 'c') => {
                let k = cmd.unwrap();
                if c.peek().is_some() {
                    return fail();
                }
                self.save_undo();
                let text = self.text()?;
                match k {
                    'a' => self.insert(a2, text),
                    'i' => self.insert(a2.max(1) - 1, text),
                    _ => {
                        range(self, a1, a2)?;
                        self.buf.drain(a1 - 1..a2);
                        self.dirty = true;
                        let n = text.len();
                        self.insert(a1 - 1, text);
                        if n == 0 {
                            self.dot = a1.min(self.dollar());
                        }
                    }
                }
                return Ok(());
            }
            Some('d') => {
                range(self, a1, a2)?;
                self.save_undo();
                self.delete(a1, a2);
            }
            Some(p @ ('p' | 'n' | 'l')) => {
                range(self, a1, a2)?;
                self.print(a1, a2, p)?;
            }
            Some('=') => {
                let n = if count > 0 { a2 } else { self.dollar() };
                let _ = writeln!(self.out, "{n}");
                return Ok(());
            }
            Some(k @ ('e' | 'E')) => {
                if count > 0 {
                    return fail();
                }
                if k == 'e' && self.dirty && !self.warned {
                    self.warned = true;
                    return fail();
                }
                let name = self.filename(&mut c, true)?;
                self.save_undo();
                self.buf.clear();
                self.marks = [None; 26];
                self.dirty = false;
                self.warned = false;
                let text = self.read_file(&name)?;
                self.insert(0, text);
                self.dirty = false;
                return Ok(());
            }
            Some('f') => {
                if count > 0 {
                    return fail();
                }
                c.skip_blanks();
                let name = c.rest();
                if !name.is_empty() {
                    self.file = Some(name);
                }
                let f = self.file.clone().unwrap_or_default();
                let _ = writeln!(self.out, "{f}");
                return Ok(());
            }
            Some('r') => {
                let name = self.filename(&mut c, false)?;
                let at = if count > 0 { a2 } else { self.dollar() };
                self.save_undo();
                let text = self.read_file(&name)?;
                self.insert(at, text);
                return Ok(());
            }
            Some(k @ ('w' | 'W')) => {
                let (a1, a2) = if count > 0 {
                    (a1, a2)
                } else {
                    (1.min(self.dollar()), self.dollar())
                };
                if count > 0 {
                    range(self, a1, a2)?;
                }
                let name = if self.stdout_w && c.peek().is_none() && self.file.is_none() {
                    None
                } else {
                    Some(self.filename(&mut c, false)?)
                };
                self.write_file(name.as_deref(), a1, a2, k == 'W')?;
                if count == 0 || (a1 <= 1 && a2 == self.dollar()) {
                    self.dirty = false;
                }
                return Ok(());
            }
            Some(k @ ('g' | 'v')) => {
                let (a1, a2) = if count > 0 {
                    (a1, a2)
                } else {
                    (1.min(self.dollar()), self.dollar())
                };
                if count > 0 {
                    range(self, a1, a2)?;
                }
                return self.global(&mut c, a1, a2, k == 'g');
            }
            Some('j') => {
                let (a1, a2) = if count < 2 { (a1, a1 + 1) } else { (a1, a2) };
                range(self, a1, a2)?;
                if a1 < a2 {
                    self.save_undo();
                    let joined: String = self.buf[a1 - 1..a2]
                        .iter()
                        .map(|l| l.text.as_str())
                        .collect();
                    self.buf.drain(a1..a2);
                    self.buf[a1 - 1].text = joined;
                    self.dirty = true;
                }
                self.dot = a1;
            }
            Some('k') => {
                let m = c
                    .next()
                    .filter(char::is_ascii_lowercase)
                    .ok_or(Fail(String::new()))?;
                if a2 == 0 || a2 > self.dollar() {
                    return fail();
                }
                self.marks[(m as u8 - b'a') as usize] = Some(self.buf[a2 - 1].id);
            }
            Some(k @ ('m' | 't')) => {
                range(self, a1, a2)?;
                let dest = self.address(&mut c)?.ok_or(Fail(String::new()))?;
                self.save_undo();
                let lines: Vec<Line> = self.buf[a1 - 1..a2].to_vec();
                let n = lines.len();
                if k == 't' {
                    let copies: Vec<String> = lines.into_iter().map(|l| l.text).collect();
                    self.insert(dest, copies);
                } else {
                    if dest >= a1 && dest < a2 {
                        return fail();
                    }
                    self.buf.drain(a1 - 1..a2);
                    let at = if dest >= a2 { dest - n } else { dest };
                    self.buf.splice(at..at, lines);
                    self.dot = at + n;
                    self.dirty = true;
                }
            }
            Some(k @ ('q' | 'Q')) => {
                if count > 0 || c.peek().is_some() {
                    return fail();
                }
                if k == 'q' && self.dirty && !self.warned {
                    self.warned = true;
                    return fail();
                }
                self.done = true;
                return Ok(());
            }
            Some('s') => {
                range(self, a1, a2)?;
                self.save_undo();
                suffix = self.substitute(&mut c, a1, a2)?;
            }
            Some('u') => {
                let Some((buf, dot)) = self.undo.take() else {
                    return fail();
                };
                self.undo = Some((std::mem::replace(&mut self.buf, buf), self.dot));
                self.dot = dot.min(self.dollar());
                self.dirty = true;
            }
            Some('!') => {
                let cmd = c.rest();
                return self.shell(&cmd);
            }
            _ => return fail(),
        }
        if suffix == ' '
            && let Some(p @ ('p' | 'l' | 'n')) = c.peek()
        {
            c.i += 1;
            suffix = p;
        }
        if c.peek().is_some() {
            return fail();
        }
        if suffix != ' ' {
            let d = self.dot;
            self.print(d, d, suffix)?;
        }
        Ok(())
    }
}

/// `t` with tabs, backspaces and backslashes escaped, and other control
/// characters in hex.
fn list(t: &str) -> String {
    let mut s = String::new();
    for c in t.chars() {
        match c {
            '\t' => s.push_str("\\t"),
            '\u{8}' => s.push_str("\\b"),
            '\\' => s.push_str("\\\\"),
            c if c.is_control() => s.push_str(&format!("\\x{:02x}", c as u32)),
            c => s.push(c),
        }
    }
    s
}

/// Expands a replacement for one match.
fn expand(rep: &str, text: &str, caps: &regexp::Captures) -> String {
    let mut out = String::new();
    let mut it = rep.chars();
    while let Some(c) = it.next() {
        match c {
            '&' => {
                if let Some(Some((a, b))) = caps.first() {
                    out.push_str(&text[*a..*b]);
                }
            }
            '\\' => match it.next() {
                Some(d @ '1'..='9') => {
                    let n = d as usize - '0' as usize;
                    if let Some(Some((a, b))) = caps.get(n) {
                        out.push_str(&text[*a..*b]);
                    }
                }
                Some(o) => out.push(o),
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}

/// The line after substituting the `nth` match (or every match from it
/// on, with `global`), or None when nothing matched.
fn subst_line(re: &Regex, text: &str, rep: &str, global: bool, nth: usize) -> Option<String> {
    let mut out = String::new();
    let mut pos = 0;
    let mut copied = 0;
    let mut seen = 0;
    let mut changed = false;
    while pos <= text.len() {
        let Some(caps) = re.find_at(text, pos) else {
            break;
        };
        let (a, b) = caps[0]?;
        seen += 1;
        if seen >= nth {
            out.push_str(&text[copied..a]);
            out.push_str(&expand(rep, text, &caps));
            copied = b;
            changed = true;
            if !global {
                break;
            }
        }
        pos = if b > a {
            b
        } else {
            match text[b..].chars().next() {
                Some(ch) => b + ch.len_utf8(),
                None => break,
            }
        };
    }
    if !changed {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut quiet = false;
    let mut ostdout = false;
    let mut file = None;
    for a in &args[1..] {
        match a.as_str() {
            "-" => quiet = true,
            "-o" => {
                ostdout = true;
                quiet = true;
            }
            s if s.starts_with('-') => return Err(Error::from("usage: ed [-] [-o] [file]")),
            s => file = Some(String::from(s)),
        }
    }
    let out: Box<dyn Write> = if ostdout {
        Box::new(io::stderr())
    } else {
        Box::new(io::stdout())
    };
    let mut ed = Ed {
        buf: Vec::new(),
        next_id: 0,
        dot: 0,
        marks: [None; 26],
        file: file.clone(),
        dirty: false,
        warned: false,
        last_re: None,
        undo: None,
        quiet,
        out,
        stdout_w: ostdout,
        input: Box::new(io::BufReader::new(io::stdin())),
        queue: Vec::new(),
        in_global: false,
        done: false,
    };
    if let Some(f) = &file {
        match ed.read_file(f) {
            Ok(text) => {
                ed.insert(0, text);
                ed.dirty = false;
            }
            Err(Fail(m)) => {
                let _ = writeln!(ed.out, "?{m}");
            }
        }
    }
    while !ed.done {
        let line = match ed.read_line() {
            Ok(Some(l)) => l,
            _ => break,
        };
        if let Err(Fail(m)) = ed.command(&line) {
            let _ = writeln!(ed.out, "?{m}");
        }
        let _ = ed.out.flush();
    }
    let _ = ed.out.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutions() {
        let re = Regex::new("o").unwrap();
        assert_eq!(subst_line(&re, "foo boo", "0", true, 1).unwrap(), "f00 b00");
        assert_eq!(
            subst_line(&re, "foo boo", "0", false, 3).unwrap(),
            "foo b0o"
        );
        let re = Regex::new("([a-z]+) ([a-z]+)").unwrap();
        assert_eq!(
            subst_line(&re, "ab cd", "\\2 \\1 &", false, 1).unwrap(),
            "cd ab ab cd"
        );
        let re = Regex::new("x*").unwrap();
        assert_eq!(subst_line(&re, "abc", "-", true, 1).unwrap(), "-a-b-c-");
        assert!(subst_line(&Regex::new("z").unwrap(), "abc", "-", true, 1).is_none());
    }
}
