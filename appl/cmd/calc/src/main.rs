//! calc [-s] [file | expression]: a calculator language with Limbo-like
//! syntax over floating point numbers, after calc(1). Input is the file,
//! the expression given as arguments, or standard input.
//!
//! An expression statement prints its value; assignments and the other
//! statements print nothing unless they call `print`. Statements are
//! `if`/`else`, `while`, `do`/`while`, `for`, `break`, `continue`,
//! `return`, `exit`, `fn name(args) { ... }`, `print expr, "text", ...`,
//! `read var, ...` and `include "file"`, with braces for grouping;
//! semicolons and newlines are optional separators.
//!
//! Operators, tightest first: `! ~ + - ++ --` (and postfix `!`, the
//! factorial), `**`, `* / % //`, `+ -`, `<< >>`, `> >= < <= <->`,
//! `== != -> <-`, `& ↑`, `^`, `| ↓`, `&&`, `||`, `? :`, the assignments
//! (`=`, `:=` and the compound ones) and `,`. Numbers may be written in C
//! (`0x1f`, `017`) or Limbo (`16r1f`) bases.
//!
//! Built in are the usual functions (`sin`, `sqrt`, `gamma`, `max`, ...;
//! those of one argument also work without brackets), the constants `e`,
//! `Pi`, `Phi`, `Gamma`, `Infinity` and `Nan`, the variables `degrees`,
//! `printbase`, `solvelimit` and `solvestep`, and the forms
//! `sigma(i = a, b, expr)`, `pi(i = a, b, expr)`, `integral(x = a, b,
//! expr)`, `differential(x = a, expr)` and `solve(equation[, var])`.
//! With `-s`, variables must be declared with `:=` before use.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, Read, Write};
use std::rc::Rc;

use purgatorio::{Context, Error};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Id(String),
    Str(String),
    Op(&'static str),
    Nl,
}

const OPS: &[&str] = &[
    "//=", "<<=", ">>=", "<->", "**", "//", "<<", ">>", "<=", ">=", "==", "!=", "->", "<-", "&&",
    "||", "++", "--", ":=", "+=", "-=", "*=", "/=", "%=", "&=", "^=", "|=", "+", "-", "*", "/",
    "%", "<", ">", "=", "!", "~", "&", "^", "|", "?", ":", ",", ";", "(", ")", "{", "}", "↑", "↓",
    "×", "÷",
];

fn digits(s: &str, radix: u32) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in s.chars() {
        v = v * f64::from(radix) + f64::from(c.to_digit(radix)?);
    }
    Some(v)
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '\n' {
            out.push(Tok::Nl);
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '#' {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
        } else if c.is_ascii_digit()
            || (c == '.' && cs.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '.') {
                // An exponent sign belongs to the number.
                if (cs[i] == 'e' || cs[i] == 'E')
                    && !cs[start..i].iter().any(|c| matches!(c, 'x' | 'X' | 'r'))
                    && matches!(cs.get(i + 1), Some('+' | '-'))
                {
                    i += 1;
                }
                i += 1;
            }
            let t: String = cs[start..i].iter().collect();
            let lower = t.to_lowercase();
            let v = if let Some(h) = lower.strip_prefix("0x") {
                digits(h, 16)
            } else if let Some((r, d)) = lower.split_once('r') {
                r.parse::<u32>()
                    .ok()
                    .filter(|r| (2..=36).contains(r))
                    .and_then(|r| digits(d, r))
            } else if lower.len() > 1
                && lower.starts_with('0')
                && lower.bytes().all(|b| (b'0'..=b'7').contains(&b))
            {
                digits(&lower[1..], 8)
            } else {
                t.parse().ok()
            };
            out.push(Tok::Num(v.ok_or_else(|| format!("bad number {t}"))?));
        } else if c.is_alphabetic() || c == '_' || c == '∫' || c == '∞' {
            let start = i;
            i += 1;
            if c != '∫' && c != '∞' {
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
            }
            out.push(Tok::Id(cs[start..i].iter().collect()));
        } else if c == '"' {
            let mut s = String::new();
            i += 1;
            while i < cs.len() && cs[i] != '"' {
                if cs[i] == '\\' && i + 1 < cs.len() {
                    i += 1;
                    s.push(match cs[i] {
                        'n' => '\n',
                        't' => '\t',
                        c => c,
                    });
                } else {
                    s.push(cs[i]);
                }
                i += 1;
            }
            if i >= cs.len() {
                return Err(String::from("unterminated string"));
            }
            i += 1;
            out.push(Tok::Str(s));
        } else {
            let rest: String = cs[i..(i + 3).min(cs.len())].iter().collect();
            let op = OPS
                .iter()
                .find(|o| rest.starts_with(**o))
                .ok_or_else(|| format!("unexpected character {c}"))?;
            out.push(Tok::Op(op));
            i += op.chars().count();
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Form {
    Sigma,
    Prod,
    Integral,
    Diff,
}

#[derive(Clone, Debug)]
enum Expr {
    Num(f64),
    Var(String),
    Unary(&'static str, Box<Expr>),
    Incr(String, f64, bool),
    Fact(Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    Assign(&'static str, String, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    Form(Form, String, Vec<Expr>),
    Solve(Box<Expr>, Option<String>),
    Seq(Box<Expr>, Box<Expr>),
}

#[derive(Debug)]
enum Stmt {
    Expr(Expr),
    Block(Vec<Stmt>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    Do(Box<Stmt>, Expr),
    For(Option<Expr>, Option<Expr>, Option<Expr>, Box<Stmt>),
    Break,
    Continue,
    Return(Option<Expr>),
    Exit,
    Fn(String, Rc<(Vec<String>, Stmt)>),
    Print(Vec<PrintItem>),
    Read(Vec<String>),
    Include(String),
    Empty,
}

#[derive(Debug)]
enum PrintItem {
    Str(String),
    Expr(Expr),
}

const FUNCS1: &[&str] = &[
    "abs", "acos", "acosh", "asin", "asinh", "atan", "atanh", "cbrt", "ceiling", "cos", "cosh",
    "erf", "exp", "floor", "frac", "gamma", "Γ", "int", "log", "log10", "round", "sign", "sin",
    "sinh", "sqrt", "tan", "tanh",
];

fn form(name: &str) -> Option<Form> {
    match name {
        "sigma" | "Σ" => Some(Form::Sigma),
        "pi" | "Π" => Some(Form::Prod),
        "integral" | "∫" => Some(Form::Integral),
        "differential" | "Δ" => Some(Form::Diff),
        _ => None,
    }
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    /// Inside brackets newlines do not end anything.
    depth: usize,
}

type PR<T> = Result<T, String>;

impl Parser {
    fn peek(&mut self) -> Option<&Tok> {
        if self.depth > 0 {
            while self.toks.get(self.pos) == Some(&Tok::Nl) {
                self.pos += 1;
            }
        }
        self.toks.get(self.pos)
    }

    fn skip_nl(&mut self) {
        while matches!(self.toks.get(self.pos), Some(Tok::Nl | Tok::Op(";"))) {
            self.pos += 1;
        }
    }

    fn is_op(&mut self, op: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if *o == op)
    }

    fn eat(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, op: &str) -> PR<()> {
        if self.eat(op) {
            return Ok(());
        }
        Err(format!("syntax error: {op} expected"))
    }

    fn is_id(&mut self, name: &str) -> bool {
        matches!(self.peek(), Some(Tok::Id(s)) if s == name)
    }

    fn ident(&mut self) -> PR<String> {
        match self.peek().cloned() {
            Some(Tok::Id(s)) => {
                self.pos += 1;
                Ok(s)
            }
            _ => Err(String::from("syntax error: name expected")),
        }
    }

    fn at_end(&mut self) -> bool {
        self.skip_nl();
        self.pos >= self.toks.len()
    }

    /// Skips to the start of the next line after an error.
    fn recover(&mut self) {
        self.depth = 0;
        while self.pos < self.toks.len() && self.toks[self.pos] != Tok::Nl {
            self.pos += 1;
        }
    }

    fn paren<T>(&mut self, f: impl FnOnce(&mut Parser) -> PR<T>) -> PR<T> {
        self.expect("(")?;
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        let r = r?;
        self.expect(")")?;
        Ok(r)
    }

    fn end_stmt(&mut self) -> PR<()> {
        match self.toks.get(self.pos) {
            None | Some(Tok::Nl | Tok::Op(";")) => {
                self.pos += usize::from(self.pos < self.toks.len());
                Ok(())
            }
            Some(Tok::Op("}")) => Ok(()),
            // `if (c) s else t` and `do s while (c)` on one line.
            Some(Tok::Id(w)) if w == "else" || w == "while" => Ok(()),
            Some(t) => Err(format!("syntax error near {t:?}")),
        }
    }

    fn body(&mut self) -> PR<Box<Stmt>> {
        self.skip_nl();
        Ok(Box::new(self.stmt()?))
    }

    fn stmt(&mut self) -> PR<Stmt> {
        self.skip_nl();
        if self.eat("{") {
            let mut v = Vec::new();
            loop {
                self.skip_nl();
                if self.eat("}") {
                    return Ok(Stmt::Block(v));
                }
                if self.pos >= self.toks.len() {
                    return Err(String::from("syntax error: } expected"));
                }
                v.push(self.stmt()?);
            }
        }
        let Some(Tok::Id(word)) = self.peek().cloned() else {
            if self.pos >= self.toks.len() {
                return Ok(Stmt::Empty);
            }
            let e = self.expr()?;
            self.end_stmt()?;
            return Ok(Stmt::Expr(e));
        };
        let keyword = matches!(
            word.as_str(),
            "if" | "while"
                | "do"
                | "for"
                | "break"
                | "continue"
                | "return"
                | "exit"
                | "fn"
                | "print"
                | "read"
                | "include"
        );
        if !keyword {
            let e = self.expr()?;
            self.end_stmt()?;
            return Ok(Stmt::Expr(e));
        }
        self.pos += 1;
        let s = match word.as_str() {
            "if" => {
                let c = self.paren(Parser::expr)?;
                let then = self.body()?;
                let save = self.pos;
                self.skip_nl();
                if self.is_id("else") {
                    self.pos += 1;
                    let e = self.body()?;
                    return Ok(Stmt::If(c, then, Some(e)));
                }
                self.pos = save;
                return Ok(Stmt::If(c, then, None));
            }
            "while" => {
                let c = self.paren(Parser::expr)?;
                return Ok(Stmt::While(c, self.body()?));
            }
            "do" => {
                let b = self.body()?;
                self.skip_nl();
                if !self.is_id("while") {
                    return Err(String::from("syntax error: while expected"));
                }
                self.pos += 1;
                let c = self.paren(Parser::expr)?;
                Stmt::Do(b, c)
            }
            "for" => {
                let (a, b, c) = self.paren(|p| {
                    let a = if p.is_op(";") { None } else { Some(p.expr()?) };
                    p.expect(";")?;
                    let b = if p.is_op(";") { None } else { Some(p.expr()?) };
                    p.expect(";")?;
                    let c = if p.is_op(")") { None } else { Some(p.expr()?) };
                    Ok((a, b, c))
                })?;
                return Ok(Stmt::For(a, b, c, self.body()?));
            }
            "break" => Stmt::Break,
            "continue" => Stmt::Continue,
            "exit" => Stmt::Exit,
            "return" => match self.toks.get(self.pos) {
                None | Some(Tok::Nl | Tok::Op(";" | "}")) => Stmt::Return(None),
                _ => Stmt::Return(Some(self.expr()?)),
            },
            "fn" => {
                let name = self.ident()?;
                let params = self.paren(|p| {
                    let mut v = Vec::new();
                    while !p.is_op(")") {
                        v.push(p.ident()?);
                        if !p.eat(",") {
                            break;
                        }
                    }
                    Ok(v)
                })?;
                self.skip_nl();
                if !self.is_op("{") {
                    return Err(String::from("syntax error: { expected"));
                }
                let body = self.stmt()?;
                return Ok(Stmt::Fn(name, Rc::new((params, body))));
            }
            "print" => {
                let mut items = Vec::new();
                loop {
                    if let Some(Tok::Str(s)) = self.peek().cloned() {
                        self.pos += 1;
                        items.push(PrintItem::Str(s));
                    } else {
                        items.push(PrintItem::Expr(self.assign()?));
                    }
                    if !self.eat(",") {
                        break;
                    }
                }
                Stmt::Print(items)
            }
            "read" => {
                let mut v = vec![self.ident()?];
                while self.eat(",") {
                    v.push(self.ident()?);
                }
                Stmt::Read(v)
            }
            _ => match self.peek().cloned() {
                Some(Tok::Str(s)) => {
                    self.pos += 1;
                    Stmt::Include(s)
                }
                _ => return Err(String::from("syntax error: file name expected")),
            },
        };
        self.end_stmt()?;
        Ok(s)
    }

    fn expr(&mut self) -> PR<Expr> {
        let mut e = self.assign()?;
        while self.eat(",") {
            let r = self.assign()?;
            e = Expr::Seq(Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn assign(&mut self) -> PR<Expr> {
        let lhs = self.cond()?;
        for op in [
            "=", ":=", "+=", "-=", "*=", "/=", "%=", "//=", "&=", "^=", "|=", "<<=", ">>=",
        ] {
            if self.eat(op) {
                let Expr::Var(name) = lhs else {
                    return Err(String::from("cannot assign to an expression"));
                };
                let rhs = self.assign()?;
                return Ok(Expr::Assign(op, name, Box::new(rhs)));
            }
        }
        Ok(lhs)
    }

    fn cond(&mut self) -> PR<Expr> {
        let c = self.binary(0)?;
        if self.eat("?") {
            let a = self.assign()?;
            self.expect(":")?;
            let b = self.cond()?;
            return Ok(Expr::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn binary(&mut self, level: usize) -> PR<Expr> {
        const LEVELS: &[&[&str]] = &[
            &["||"],
            &["&&"],
            &["|", "↓"],
            &["^"],
            &["&", "↑"],
            &["==", "!=", "->", "<-"],
            &["<", "<=", ">", ">=", "<->"],
            &["<<", ">>"],
            &["+", "-"],
            &["*", "/", "%", "//", "×", "÷"],
        ];
        if level == LEVELS.len() {
            return self.power();
        }
        let mut e = self.binary(level + 1)?;
        'outer: loop {
            for op in LEVELS[level] {
                if self.eat(op) {
                    let r = self.binary(level + 1)?;
                    e = Expr::Bin(op, Box::new(e), Box::new(r));
                    continue 'outer;
                }
            }
            return Ok(e);
        }
    }

    fn power(&mut self) -> PR<Expr> {
        let b = self.unary()?;
        if self.eat("**") {
            let e = self.power()?;
            return Ok(Expr::Bin("**", Box::new(b), Box::new(e)));
        }
        Ok(b)
    }

    fn unary(&mut self) -> PR<Expr> {
        for op in ["!", "~", "+", "-"] {
            if self.eat(op) {
                return Ok(Expr::Unary(op, Box::new(self.unary()?)));
            }
        }
        for (op, d) in [("++", 1.0), ("--", -1.0)] {
            if self.eat(op) {
                return Ok(Expr::Incr(self.ident()?, d, true));
            }
        }
        let mut e = self.primary()?;
        loop {
            if self.eat("!") {
                e = Expr::Fact(Box::new(e));
            } else if let Expr::Var(name) = &e
                && (self.is_op("++") || self.is_op("--"))
            {
                let d = if self.eat("++") { 1.0 } else { -1.0 };
                self.eat("--");
                e = Expr::Incr(name.clone(), d, false);
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> PR<Expr> {
        match self.peek().cloned() {
            Some(Tok::Num(v)) => {
                self.pos += 1;
                Ok(Expr::Num(v))
            }
            Some(Tok::Op("(")) => self.paren(Parser::expr),
            Some(Tok::Id(name)) => {
                self.pos += 1;
                if let Some(f) = form(&name)
                    && self.is_op("(")
                {
                    let args = self.paren(|p| {
                        let first = p.assign()?;
                        let Expr::Assign("=", var, start) = first else {
                            return Err(String::from("syntax error: var = value expected"));
                        };
                        let mut v = vec![*start];
                        while p.eat(",") {
                            v.push(p.assign()?);
                        }
                        Ok((var, v))
                    })?;
                    let want = if f == Form::Diff { 2 } else { 3 };
                    if args.1.len() != want {
                        return Err(format!("{name}: wrong number of arguments"));
                    }
                    return Ok(Expr::Form(f, args.0, args.1));
                }
                if name == "solve" {
                    let (eq, var) = self.paren(|p| {
                        let eq = p.assign()?;
                        let var = if p.eat(",") { Some(p.ident()?) } else { None };
                        Ok((eq, var))
                    })?;
                    return Ok(Expr::Solve(Box::new(eq), var));
                }
                if self.is_op("(") {
                    let args = self.paren(|p| {
                        let mut v = Vec::new();
                        while !p.is_op(")") {
                            v.push(p.assign()?);
                            if !p.eat(",") {
                                break;
                            }
                        }
                        Ok(v)
                    })?;
                    return Ok(Expr::Call(name, args));
                }
                if FUNCS1.contains(&name.as_str()) {
                    let arg = self.unary()?;
                    return Ok(Expr::Call(name, vec![arg]));
                }
                Ok(Expr::Var(name))
            }
            Some(t) => Err(format!("syntax error near {t:?}")),
            None => Err(String::from("syntax error: unexpected end of input")),
        }
    }
}

enum Flow {
    Normal,
    Break,
    Continue,
    Return(f64),
    Exit,
}

struct Calc {
    globals: HashMap<String, f64>,
    frames: Vec<HashMap<String, f64>>,
    fns: HashMap<String, Rc<(Vec<String>, Stmt)>>,
    strict: bool,
    seed: u64,
    out: io::BufWriter<io::Stdout>,
}

fn int(v: f64) -> i64 {
    v as i64
}

fn truth(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

fn gamma(x: f64) -> f64 {
    if x == x.floor() && x > 0.0 && x < 171.0 {
        return (1..x as u64).map(|i| i as f64).product();
    }
    if x < 0.5 {
        return std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * gamma(1.0 - x));
    }
    const P: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    let x = x - 1.0;
    let mut a = P[0];
    let t = x + 7.5;
    for (i, p) in P.iter().enumerate().skip(1) {
        a += p / (x + i as f64);
    }
    (2.0 * std::f64::consts::PI).sqrt() * t.powf(x + 0.5) * (-t).exp() * a
}

fn erf(x: f64) -> f64 {
    if x.abs() < 2.5 {
        // The Maclaurin series.
        let mut sum = 0.0f64;
        let mut term = x;
        let mut n = 0.0;
        while term.abs() > 1e-17 * sum.abs().max(1e-300) || n < 3.0 {
            sum += term / (2.0 * n + 1.0);
            n += 1.0;
            term *= -x * x / n;
            if n > 200.0 {
                break;
            }
        }
        return sum * 2.0 / std::f64::consts::PI.sqrt();
    }
    // erfc by its continued fraction, evaluated from the tail.
    let ax = x.abs();
    let mut f = 0.0;
    for k in (1..60).rev() {
        f = (k as f64 / 2.0) / (ax + f);
    }
    let erfc = (-ax * ax).exp() / std::f64::consts::PI.sqrt() / (ax + f);
    (1.0 - erfc).copysign(x)
}

/// A value as calc prints it.
fn show(v: f64, base: f64) -> String {
    if v.is_nan() {
        return String::from("NaN");
    }
    if v.is_infinite() {
        return String::from(if v > 0.0 { "Infinity" } else { "-Infinity" });
    }
    let b = base as u32;
    if b != 10 && (2..=36).contains(&b) && v == v.trunc() && v.abs() < 9.0e15 {
        let mut n = v.abs() as u64;
        let mut ds = Vec::new();
        loop {
            ds.push(std::char::from_digit((n % u64::from(b)) as u32, b).unwrap_or('?'));
            n /= u64::from(b);
            if n == 0 {
                break;
            }
        }
        let s: String = ds.iter().rev().collect();
        return format!("{}{b}r{s}", if v < 0.0 { "-" } else { "" });
    }
    if v != 0.0 && (v.abs() >= 1e21 || v.abs() < 1e-6) {
        return format!("{v:e}");
    }
    format!("{v}")
}

impl Calc {
    fn get(&mut self, name: &str) -> Result<f64, String> {
        if let Some(f) = self.frames.last()
            && let Some(v) = f.get(name)
        {
            return Ok(*v);
        }
        if let Some(v) = self.globals.get(name) {
            return Ok(*v);
        }
        if self.strict {
            return Err(format!("undeclared variable {name}"));
        }
        self.declare(name, 0.0);
        Ok(0.0)
    }

    fn declare(&mut self, name: &str, v: f64) {
        match self.frames.last_mut() {
            Some(f) => f.insert(String::from(name), v),
            None => self.globals.insert(String::from(name), v),
        };
    }

    fn set(&mut self, name: &str, v: f64) -> Result<(), String> {
        if let Some(f) = self.frames.last_mut()
            && let Some(slot) = f.get_mut(name)
        {
            *slot = v;
            return Ok(());
        }
        if let Some(slot) = self.globals.get_mut(name) {
            *slot = v;
            return Ok(());
        }
        if self.strict {
            return Err(format!("undeclared variable {name}"));
        }
        self.declare(name, v);
        Ok(())
    }

    fn angle_in(&self, v: f64) -> f64 {
        if self.globals.get("degrees").is_some_and(|d| *d != 0.0) {
            v.to_radians()
        } else {
            v
        }
    }

    fn angle_out(&self, v: f64) -> f64 {
        if self.globals.get("degrees").is_some_and(|d| *d != 0.0) {
            v.to_degrees()
        } else {
            v
        }
    }

    fn rand(&mut self) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 11) as f64 / (1u64 << 53) as f64
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Result<f64, String> {
        let mut vs = Vec::with_capacity(args.len());
        for a in args {
            vs.push(self.eval(a)?);
        }
        if let Some(f) = self.fns.get(name).cloned() {
            if f.0.len() != vs.len() {
                return Err(format!("{name}: wrong number of arguments"));
            }
            self.frames.push(f.0.iter().cloned().zip(vs).collect());
            if self.frames.len() > 2000 {
                self.frames.pop();
                return Err(String::from("recursion too deep"));
            }
            let r = self.exec(&f.1);
            self.frames.pop();
            return match r? {
                Flow::Return(v) => Ok(v),
                Flow::Exit => Err(String::from("exit")),
                _ => Ok(0.0),
            };
        }
        let one = |vs: &[f64]| -> Result<f64, String> {
            match vs {
                [x] => Ok(*x),
                _ => Err(format!("{name}: wrong number of arguments")),
            }
        };
        let two = |vs: &[f64]| -> Result<(f64, f64), String> {
            match vs {
                [x, y] => Ok((*x, *y)),
                _ => Err(format!("{name}: wrong number of arguments")),
            }
        };
        Ok(match name {
            "abs" => one(&vs)?.abs(),
            "acos" => self.angle_out(one(&vs)?.acos()),
            "acosh" => one(&vs)?.acosh(),
            "asin" => self.angle_out(one(&vs)?.asin()),
            "asinh" => one(&vs)?.asinh(),
            "atan" => self.angle_out(one(&vs)?.atan()),
            "atanh" => one(&vs)?.atanh(),
            "atan2" => {
                let (y, x) = two(&vs)?;
                self.angle_out(y.atan2(x))
            }
            "cbrt" => one(&vs)?.cbrt(),
            "ceiling" => one(&vs)?.ceil(),
            "cos" => self.angle_in(one(&vs)?).cos(),
            "cosh" => one(&vs)?.cosh(),
            "erf" => erf(one(&vs)?),
            "exp" => one(&vs)?.exp(),
            "floor" => one(&vs)?.floor(),
            "frac" => one(&vs)?.fract(),
            "gamma" | "Γ" => gamma(one(&vs)?),
            "int" => one(&vs)?.trunc(),
            "log" => one(&vs)?.ln(),
            "log10" => one(&vs)?.log10(),
            "max" if !vs.is_empty() => vs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            "min" if !vs.is_empty() => vs.iter().copied().fold(f64::INFINITY, f64::min),
            "pow" => {
                let (x, y) = two(&vs)?;
                x.powf(y)
            }
            "rand" => self.rand(),
            "round" => one(&vs)?.round(),
            "sign" => {
                let x = one(&vs)?;
                if x > 0.0 {
                    1.0
                } else if x < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }
            "sin" => self.angle_in(one(&vs)?).sin(),
            "sinh" => one(&vs)?.sinh(),
            "sqrt" => one(&vs)?.sqrt(),
            "tan" => self.angle_in(one(&vs)?).tan(),
            "tanh" => one(&vs)?.tanh(),
            _ => return Err(format!("unknown function {name}")),
        })
    }

    /// Evaluates `body` with `var` bound to `x`, restoring `var` after.
    fn with_var(&mut self, var: &str, x: f64, body: &Expr) -> Result<f64, String> {
        let old = self
            .frames
            .last()
            .and_then(|f| f.get(var))
            .or(self.globals.get(var))
            .copied();
        self.set(var, x)?;
        let r = self.eval(body);
        match old {
            Some(o) => self.set(var, o)?,
            None => {
                if let Some(f) = self.frames.last_mut() {
                    f.remove(var);
                } else {
                    self.globals.remove(var);
                }
            }
        }
        r
    }

    fn form(&mut self, f: Form, var: &str, args: &[Expr]) -> Result<f64, String> {
        let a = self.eval(&args[0])?;
        match f {
            Form::Sigma | Form::Prod => {
                let b = self.eval(&args[1])?;
                let mut acc = if f == Form::Sigma { 0.0 } else { 1.0 };
                let mut i = a;
                while i <= b {
                    let v = self.with_var(var, i, &args[2])?;
                    if f == Form::Sigma {
                        acc += v;
                    } else {
                        acc *= v;
                    }
                    i += 1.0;
                }
                Ok(acc)
            }
            Form::Integral => {
                let b = self.eval(&args[1])?;
                // Composite Simpson's rule.
                let n = 2000;
                let h = (b - a) / f64::from(n);
                let mut s = self.with_var(var, a, &args[2])? + self.with_var(var, b, &args[2])?;
                for k in 1..n {
                    let w = if k % 2 == 1 { 4.0 } else { 2.0 };
                    s += w * self.with_var(var, a + f64::from(k) * h, &args[2])?;
                }
                Ok(s * h / 3.0)
            }
            Form::Diff => {
                let h = 1e-3 * a.abs().max(1.0);
                let mut f = |x: f64| self.with_var(var, x, &args[1]);
                let d = (f(a - 2.0 * h)? - 8.0 * f(a - h)? + 8.0 * f(a + h)? - f(a + 2.0 * h)?)
                    / (12.0 * h);
                // The estimate is good to about ten digits; round there so
                // that exact answers print exactly.
                if d == 0.0 || !d.is_finite() {
                    return Ok(d);
                }
                let mag = 10f64.powi(10 - d.abs().log10().floor() as i32);
                Ok((d * mag).round() / mag)
            }
        }
    }

    fn vars(e: &Expr, out: &mut BTreeSet<String>) {
        match e {
            Expr::Var(n) => {
                out.insert(n.clone());
            }
            Expr::Num(_) => {}
            Expr::Unary(_, a) | Expr::Fact(a) => Calc::vars(a, out),
            Expr::Bin(_, a, b) | Expr::Seq(a, b) => {
                Calc::vars(a, out);
                Calc::vars(b, out);
            }
            Expr::Cond(a, b, c) => {
                Calc::vars(a, out);
                Calc::vars(b, out);
                Calc::vars(c, out);
            }
            Expr::Call(_, args) | Expr::Form(_, _, args) => {
                args.iter().for_each(|a| Calc::vars(a, out))
            }
            Expr::Assign(_, _, a) | Expr::Solve(a, _) => Calc::vars(a, out),
            Expr::Incr(..) => {}
        }
    }

    fn solve(&mut self, eq: &Expr, var: &Option<String>) -> Result<f64, String> {
        let f = match eq {
            Expr::Bin("==", a, b) => Expr::Bin("-", a.clone(), b.clone()),
            e => e.clone(),
        };
        let var = match var {
            Some(v) => v.clone(),
            None => {
                let mut vs = BTreeSet::new();
                Calc::vars(&f, &mut vs);
                let consts = [
                    "e", "Pi", "π", "Phi", "φ", "Gamma", "γ", "Infinity", "∞", "Nan",
                ];
                let free: Vec<String> = vs
                    .into_iter()
                    .filter(|v| !consts.contains(&v.as_str()) && !self.globals.contains_key(v))
                    .collect();
                match free.as_slice() {
                    [v] => v.clone(),
                    _ => return Err(String::from("solve: which variable?")),
                }
            }
        };
        let limit = self.globals.get("solvelimit").copied().unwrap_or(100.0);
        let step = self.globals.get("solvestep").copied().unwrap_or(1.0);
        if step <= 0.0 {
            return Err(String::from("solve: bad solvestep"));
        }
        let mut roots: Vec<f64> = Vec::new();
        let mut x = -limit;
        let mut fx = self.with_var(&var, x, &f)?;
        while x < limit {
            let nx = (x + step).min(limit);
            let fn_ = self.with_var(&var, nx, &f)?;
            if fx == 0.0 {
                roots.push(x);
            } else if fn_ != 0.0 && fx.signum() != fn_.signum() && fx.is_finite() && fn_.is_finite()
            {
                let (mut lo, mut hi, mut flo) = (x, nx, fx);
                for _ in 0..200 {
                    let mid = (lo + hi) / 2.0;
                    let fm = self.with_var(&var, mid, &f)?;
                    if fm == 0.0 || (hi - lo).abs() < 1e-15 * mid.abs().max(1.0) {
                        lo = mid;
                        hi = mid;
                        break;
                    }
                    if fm.signum() == flo.signum() {
                        lo = mid;
                        flo = fm;
                    } else {
                        hi = mid;
                    }
                }
                roots.push((lo + hi) / 2.0);
            }
            x = nx;
            fx = fn_;
        }
        if fx == 0.0 {
            roots.push(x);
        }
        let base = self.globals.get("printbase").copied().unwrap_or(10.0);
        for r in &roots {
            let _ = writeln!(self.out, "{}", show(*r, base));
        }
        roots
            .last()
            .copied()
            .ok_or_else(|| String::from("solve: no solution"))
    }

    fn eval(&mut self, e: &Expr) -> Result<f64, String> {
        Ok(match e {
            Expr::Num(v) => *v,
            Expr::Var(n) => self.get(n)?,
            Expr::Unary(op, a) => {
                let v = self.eval(a)?;
                match *op {
                    "!" => truth(v == 0.0),
                    "~" => !int(v) as f64,
                    "-" => -v,
                    _ => v,
                }
            }
            Expr::Incr(n, d, pre) => {
                let old = self.get(n)?;
                self.set(n, old + d)?;
                if *pre { old + d } else { old }
            }
            Expr::Fact(a) => gamma(self.eval(a)? + 1.0),
            Expr::Bin(op, a, b) => {
                let x = self.eval(a)?;
                match *op {
                    "&&" => return Ok(truth(x != 0.0 && self.eval(b)? != 0.0)),
                    "||" => return Ok(truth(x != 0.0 || self.eval(b)? != 0.0)),
                    _ => {}
                }
                let y = self.eval(b)?;
                binop(op, x, y)
            }
            Expr::Assign(op, n, rhs) => {
                let v = self.eval(rhs)?;
                let v = match *op {
                    "=" => v,
                    ":=" => {
                        self.declare(n, v);
                        return Ok(v);
                    }
                    _ => binop(&op[..op.len() - 1], self.get(n)?, v),
                };
                self.set(n, v)?;
                v
            }
            Expr::Cond(c, a, b) => {
                if self.eval(c)? != 0.0 {
                    self.eval(a)?
                } else {
                    self.eval(b)?
                }
            }
            Expr::Call(n, args) => self.call(n, args)?,
            Expr::Form(f, var, args) => self.form(*f, var, args)?,
            Expr::Solve(eq, var) => self.solve(eq, var)?,
            Expr::Seq(a, b) => {
                self.eval(a)?;
                self.eval(b)?
            }
        })
    }

    fn exec(&mut self, s: &Stmt) -> Result<Flow, String> {
        match s {
            Stmt::Expr(e) => {
                let v = self.eval(e)?;
                if !matches!(e, Expr::Assign(..) | Expr::Incr(..)) && self.frames.is_empty() {
                    let base = self.globals.get("printbase").copied().unwrap_or(10.0);
                    let _ = writeln!(self.out, "{}", show(v, base));
                }
            }
            Stmt::Block(v) => {
                for s in v {
                    match self.exec(s)? {
                        Flow::Normal => {}
                        f => return Ok(f),
                    }
                }
            }
            Stmt::If(c, a, b) => {
                if self.eval(c)? != 0.0 {
                    return self.exec(a);
                } else if let Some(b) = b {
                    return self.exec(b);
                }
            }
            Stmt::While(c, body) => {
                while self.eval(c)? != 0.0 {
                    match self.exec(body)? {
                        Flow::Break => break,
                        f @ (Flow::Return(_) | Flow::Exit) => return Ok(f),
                        _ => {}
                    }
                }
            }
            Stmt::Do(body, c) => loop {
                match self.exec(body)? {
                    Flow::Break => break,
                    f @ (Flow::Return(_) | Flow::Exit) => return Ok(f),
                    _ => {}
                }
                if self.eval(c)? == 0.0 {
                    break;
                }
            },
            Stmt::For(a, c, step, body) => {
                if let Some(a) = a {
                    self.eval(a)?;
                }
                while match c {
                    Some(c) => self.eval(c)? != 0.0,
                    None => true,
                } {
                    match self.exec(body)? {
                        Flow::Break => break,
                        f @ (Flow::Return(_) | Flow::Exit) => return Ok(f),
                        _ => {}
                    }
                    if let Some(s) = step {
                        self.eval(s)?;
                    }
                }
            }
            Stmt::Break => return Ok(Flow::Break),
            Stmt::Continue => return Ok(Flow::Continue),
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e)?,
                    None => 0.0,
                };
                return Ok(Flow::Return(v));
            }
            Stmt::Exit => return Ok(Flow::Exit),
            Stmt::Fn(name, f) => {
                self.fns.insert(name.clone(), f.clone());
            }
            Stmt::Print(items) => {
                let base = self.globals.get("printbase").copied().unwrap_or(10.0);
                for it in items {
                    match it {
                        PrintItem::Str(s) => {
                            let _ = write!(self.out, "{s}");
                        }
                        PrintItem::Expr(e) => {
                            let v = self.eval(e)?;
                            let _ = write!(self.out, "{}", show(v, base));
                        }
                    }
                }
            }
            Stmt::Read(vars) => {
                let _ = self.out.flush();
                for v in vars {
                    let mut l = String::new();
                    io::stdin()
                        .lock()
                        .read_line(&mut l)
                        .map_err(|e| e.to_string())?;
                    let x = self.value(l.trim())?;
                    self.set(v, x)?;
                }
            }
            Stmt::Include(f) => {
                let src = std::fs::read_to_string(f)
                    .map_err(|_| format!("cannot open {f}: {}", purgatorio::errstr()))?;
                if let Flow::Exit = self.run(&src) {
                    return Ok(Flow::Exit);
                }
            }
            Stmt::Empty => {}
        }
        Ok(Flow::Normal)
    }

    fn value(&mut self, s: &str) -> Result<f64, String> {
        let mut p = Parser {
            toks: lex(s)?,
            pos: 0,
            depth: 0,
        };
        let e = p.expr()?;
        self.eval(&e)
    }

    /// Runs a program, reporting errors and going on with the next line.
    fn run(&mut self, src: &str) -> Flow {
        let toks = match lex(src) {
            Ok(t) => t,
            Err(e) => {
                let _ = self.out.flush();
                eprintln!("calc: {e}");
                return Flow::Normal;
            }
        };
        let mut p = Parser {
            toks,
            pos: 0,
            depth: 0,
        };
        while !p.at_end() {
            let s = match p.stmt() {
                Ok(s) => s,
                Err(e) => {
                    let _ = self.out.flush();
                    eprintln!("calc: {e}");
                    p.recover();
                    continue;
                }
            };
            match self.exec(&s) {
                Ok(Flow::Exit) => return Flow::Exit,
                Ok(_) => {}
                Err(e) => {
                    let _ = self.out.flush();
                    eprintln!("calc: {e}");
                }
            }
        }
        Flow::Normal
    }
}

fn binop(op: &str, x: f64, y: f64) -> f64 {
    match op {
        "+" => x + y,
        "-" => x - y,
        "*" | "×" => x * y,
        "/" | "÷" => x / y,
        "%" => x % y,
        "//" => (x / y).trunc(),
        "**" => x.powf(y),
        "<<" => (int(x) << (int(y) & 63)) as f64,
        ">>" => (int(x) >> (int(y) & 63)) as f64,
        "<" => truth(x < y),
        "<=" => truth(x <= y),
        ">" => truth(x > y),
        ">=" => truth(x >= y),
        "<->" => truth((x != 0.0) == (y != 0.0)),
        "==" => truth(x == y),
        "!=" => truth(x != y),
        "->" => truth(x == 0.0 || y != 0.0),
        "<-" => truth(x != 0.0 || y == 0.0),
        "&" => (int(x) & int(y)) as f64,
        "↑" => truth(!(x != 0.0 && y != 0.0)),
        "^" => (int(x) ^ int(y)) as f64,
        "|" => (int(x) | int(y)) as f64,
        "↓" => truth(!(x != 0.0 || y != 0.0)),
        _ => f64::NAN,
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut rest = &args[1.min(args.len())..];
    let mut strict = false;
    if rest.first().map(String::as_str) == Some("-s") {
        strict = true;
        rest = &rest[1..];
    }
    let src = match rest {
        [] => {
            let mut s = String::new();
            io::stdin().lock().read_to_string(&mut s)?;
            s
        }
        [f] if purgatorio::stat(f).is_ok() => std::fs::read_to_string(f)
            .map_err(|_| Error(format!("cannot open {f}: {}", purgatorio::errstr())))?,
        words => words.join(" "),
    };
    let mut globals = HashMap::new();
    for (k, v) in [
        ("e", std::f64::consts::E),
        ("Pi", std::f64::consts::PI),
        ("π", std::f64::consts::PI),
        ("Phi", std::f64::consts::GOLDEN_RATIO),
        ("φ", std::f64::consts::GOLDEN_RATIO),
        ("Gamma", std::f64::consts::EULER_GAMMA),
        ("γ", std::f64::consts::EULER_GAMMA),
        ("Infinity", f64::INFINITY),
        ("∞", f64::INFINITY),
        ("Nan", f64::NAN),
        ("degrees", 0.0),
        ("printbase", 10.0),
        ("solvelimit", 100.0),
        ("solvestep", 1.0),
    ] {
        globals.insert(String::from(k), v);
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64)
        | 1;
    let mut c = Calc {
        globals,
        frames: Vec::new(),
        fns: HashMap::new(),
        strict,
        seed,
        out: io::BufWriter::new(io::stdout()),
    };
    c.run(&src);
    c.out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(src: &str) -> Vec<String> {
        let mut globals = HashMap::new();
        globals.insert(String::from("printbase"), 10.0);
        globals.insert(String::from("Pi"), std::f64::consts::PI);
        let mut c = Calc {
            globals,
            frames: Vec::new(),
            fns: HashMap::new(),
            strict: false,
            seed: 1,
            out: io::BufWriter::new(io::stdout()),
        };
        let mut p = Parser {
            toks: lex(src).unwrap(),
            pos: 0,
            depth: 0,
        };
        let mut out = Vec::new();
        while !p.at_end() {
            let s = p.stmt().unwrap();
            if let Stmt::Expr(e) = &s
                && !matches!(e, Expr::Assign(..) | Expr::Incr(..))
            {
                out.push(show(c.eval(e).unwrap(), 10.0));
            } else {
                c.exec(&s).unwrap();
            }
        }
        out
    }

    #[test]
    fn expressions() {
        assert_eq!(
            calc("1+2*3; 2**3**2; -2**2; 7//2; 7%3; 5!"),
            ["7", "512", "4", "3", "1", "120"]
        );
        assert_eq!(calc("0x1f + 8r17 + 017"), ["61"]);
        assert_eq!(calc("x = 3\nx * x\nx += 1; x"), ["9", "4"]);
        assert_eq!(
            calc("1 < 2 && 2 < 1; 1 ? 5 : 6; 0 -> 1; 6 & 3; 6 ^ 3"),
            ["0", "5", "1", "2", "5"]
        );
        assert_eq!(calc("sigma(i = 0, 100, 1/i!)"), ["2.7182818284590455"]);
        assert_eq!(calc("differential(x=1, x*x+5*x+6)"), ["7"]);
        assert_eq!(calc("fn sq(a) { return a*a }\nsq(12)"), ["144"]);
        assert_eq!(calc("sqrt 16 + 1"), ["5"]);
        assert_eq!(calc("pi(i = 1, 5, i)"), ["120"]);
    }

    #[test]
    fn special_functions() {
        assert!((gamma(0.5) - std::f64::consts::PI.sqrt()).abs() < 1e-14);
        assert!((erf(1.0) - 0.842_700_792_949_714_9).abs() < 1e-15);
        assert!((erf(3.0) - 0.999_977_909_503_001_4).abs() < 1e-15);
    }
}
