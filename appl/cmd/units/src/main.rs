//! units [-v] [file]: convert quantities between units. It asks `you
//! have:` and `you want:` on standard input and prints the factors that
//! convert one to the other, `* f` and `/ 1/f`. An empty `you want:` shows
//! the quantity in fundamental units. Units, prefixes and constants come
//! from `/lib/units` or `file`; `-v` prints that database.
//!
//! A quantity multiplies units and numbers. From loosest to tightest
//! binding: `+ -`; `* / × ÷`; juxtaposition, which multiplies; `² ³ ^` with
//! an integer power; `|`, which divides numbers; and parentheses. A name
//! is looked up as written, then as a prefix and a unit, then without a
//! plural `s` or `es`. Only multiplicative conversions are done.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

/// A quantity: a factor times fundamental units to integer powers.
#[derive(Clone, Debug)]
struct Q {
    f: f64,
    dims: Vec<i32>,
}

impl Q {
    fn num(f: f64, n: usize) -> Q {
        Q {
            f,
            dims: vec![0; n],
        }
    }

    fn mul(mut self, o: &Q, sign: i32) -> Q {
        if sign > 0 {
            self.f *= o.f;
        } else {
            self.f /= o.f;
        }
        for (a, b) in self.dims.iter_mut().zip(&o.dims) {
            *a += sign * b;
        }
        self
    }

    fn pow(mut self, n: i32) -> Q {
        self.f = self.f.powi(n);
        for d in &mut self.dims {
            *d *= n;
        }
        self
    }

    fn dimensionless(&self) -> bool {
        self.dims.iter().all(|d| *d == 0)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Name(String),
    Op(char),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit()
            || (c == '.' && cs.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            if i < cs.len() && (cs[i] == 'e' || cs[i] == 'E') {
                let mut j = i + 1;
                if j < cs.len() && (cs[j] == '+' || cs[j] == '-') {
                    j += 1;
                }
                if j < cs.len() && cs[j].is_ascii_digit() {
                    i = j;
                    while i < cs.len() && cs[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let t: String = cs[start..i].iter().collect();
            out.push(Tok::Num(t.parse().map_err(|_| format!("bad number {t}"))?));
        } else if "+-*/×÷^²³|()".contains(c) {
            out.push(Tok::Op(c));
            i += 1;
        } else if c.is_alphabetic() || c == '_' || c == 'µ' {
            let start = i;
            while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') && !"²³".contains(cs[i])
            {
                i += 1;
            }
            out.push(Tok::Name(cs[start..i].iter().collect()));
        } else {
            return Err(format!("unexpected character {c}"));
        }
    }
    Ok(out)
}

struct Db {
    /// Definitions as written, by name; prefixes keep their `-`.
    defs: HashMap<String, String>,
    order: Vec<String>,
    bases: Vec<String>,
    cache: HashMap<String, Q>,
    depth: usize,
}

impl Db {
    fn load(text: &str) -> Db {
        let mut db = Db {
            defs: HashMap::new(),
            order: Vec::new(),
            bases: Vec::new(),
            cache: HashMap::new(),
            depth: 0,
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, val) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let val = val.trim();
            if val == "!" {
                db.bases.push(String::from(name));
            }
            if !db.defs.contains_key(name) {
                db.order.push(String::from(name));
            }
            db.defs.insert(String::from(name), String::from(val));
        }
        db
    }

    fn eval_def(&mut self, name: &str, def: &str) -> Result<Q, String> {
        if let Some(q) = self.cache.get(name) {
            return Ok(q.clone());
        }
        let n = self.bases.len();
        let q = if def == "!" {
            let mut q = Q::num(1.0, n);
            let i = self.bases.iter().position(|b| b == name).unwrap_or(0);
            q.dims[i] = 1;
            q
        } else {
            if self.depth > 50 {
                return Err(format!("{name}: definition loops"));
            }
            self.depth += 1;
            let r = self.parse(def);
            self.depth -= 1;
            r?
        };
        self.cache.insert(String::from(name), q.clone());
        Ok(q)
    }

    fn exact(&mut self, name: &str) -> Option<Result<Q, String>> {
        let def = self.defs.get(name)?.clone();
        Some(self.eval_def(name, &def))
    }

    fn plain(&mut self, name: &str) -> Option<Result<Q, String>> {
        if let Some(r) = self.exact(name) {
            return Some(r);
        }
        for suffix in ["s", "es"] {
            if let Some(base) = name.strip_suffix(suffix)
                && !base.is_empty()
                && let Some(r) = self.exact(base)
            {
                return Some(r);
            }
        }
        None
    }

    fn lookup(&mut self, name: &str) -> Result<Q, String> {
        if let Some(r) = self.exact(name) {
            return r;
        }
        let mut prefixes: Vec<String> = self
            .defs
            .keys()
            .filter_map(|k| k.strip_suffix('-').map(String::from))
            .filter(|p| !p.is_empty() && name.starts_with(p.as_str()) && name.len() > p.len())
            .collect();
        prefixes.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
        for p in prefixes {
            let rest = &name[p.len()..];
            if let Some(r) = self.plain(rest) {
                let unit = r?;
                let def = self.defs[&format!("{p}-")].clone();
                let pre = self.eval_def(&format!("{p}-"), &def)?;
                return Ok(pre.mul(&unit, 1));
            }
        }
        match self.plain(name) {
            Some(r) => r,
            None => Err(format!("unknown unit: {name}")),
        }
    }

    fn parse(&mut self, s: &str) -> Result<Q, String> {
        let toks = lex(s)?;
        let mut p = Parser { toks, pos: 0 };
        let q = p.sum(self)?;
        if p.pos < p.toks.len() {
            return Err(format!("syntax error in {s}"));
        }
        Ok(q)
    }

    /// `q` in fundamental units, as `f m kg^-1`.
    fn show(&self, q: &Q) -> String {
        let mut s = g(q.f, 7);
        for (b, &d) in self.bases.iter().zip(&q.dims) {
            match d {
                0 => {}
                1 => s.push_str(&format!(" {b}")),
                _ => s.push_str(&format!(" {b}^{d}")),
            }
        }
        s
    }
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn op(&mut self, set: &str) -> Option<char> {
        if let Some(Tok::Op(c)) = self.peek()
            && set.contains(*c)
        {
            let c = *c;
            self.pos += 1;
            return Some(c);
        }
        None
    }

    fn sum(&mut self, db: &mut Db) -> Result<Q, String> {
        let mut q = self.product(db)?;
        while let Some(c) = self.op("+-") {
            let r = self.product(db)?;
            if q.dims != r.dims {
                return Err(String::from("conformability"));
            }
            q.f = if c == '+' { q.f + r.f } else { q.f - r.f };
        }
        Ok(q)
    }

    fn product(&mut self, db: &mut Db) -> Result<Q, String> {
        let mut q = self.cat(db)?;
        while let Some(c) = self.op("*/×÷") {
            let r = self.cat(db)?;
            q = q.mul(&r, if c == '*' || c == '×' { 1 } else { -1 });
        }
        Ok(q)
    }

    fn starts_primary(&self) -> bool {
        matches!(self.peek(), Some(Tok::Num(_) | Tok::Name(_) | Tok::Op('(')))
    }

    fn cat(&mut self, db: &mut Db) -> Result<Q, String> {
        let mut q = self.power(db)?;
        while self.starts_primary() {
            let r = self.power(db)?;
            q = q.mul(&r, 1);
        }
        Ok(q)
    }

    fn power(&mut self, db: &mut Db) -> Result<Q, String> {
        let mut q = self.bar(db)?;
        loop {
            if self.op("²").is_some() {
                q = q.pow(2);
            } else if self.op("³").is_some() {
                q = q.pow(3);
            } else if self.op("^").is_some() {
                let neg = self.op("-").is_some();
                let e = self.bar(db)?;
                if !e.dimensionless() || e.f.fract() != 0.0 {
                    return Err(String::from("exponent must be an integer"));
                }
                let n = e.f as i32;
                q = q.pow(if neg { -n } else { n });
            } else {
                return Ok(q);
            }
        }
    }

    fn bar(&mut self, db: &mut Db) -> Result<Q, String> {
        let mut q = self.primary(db)?;
        while self.op("|").is_some() {
            let r = self.primary(db)?;
            q = q.mul(&r, -1);
        }
        Ok(q)
    }

    fn primary(&mut self, db: &mut Db) -> Result<Q, String> {
        let n = db.bases.len();
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Num(f)) => {
                self.pos += 1;
                Ok(Q::num(f, n))
            }
            Some(Tok::Name(s)) => {
                self.pos += 1;
                db.lookup(&s)
            }
            Some(Tok::Op('(')) => {
                self.pos += 1;
                let q = self.sum(db)?;
                if self.op(")").is_none() {
                    return Err(String::from("missing )"));
                }
                Ok(q)
            }
            Some(Tok::Op('-')) => {
                self.pos += 1;
                let mut q = self.primary(db)?;
                q.f = -q.f;
                Ok(q)
            }
            Some(Tok::Op('/')) => {
                // A leading `/unit` is its reciprocal.
                self.pos += 1;
                Ok(Q::num(1.0, n).mul(&self.cat(db)?, -1))
            }
            _ => Err(String::from("syntax error")),
        }
    }
}

/// C's `%.Ng`.
fn g(x: f64, prec: usize) -> String {
    if x == 0.0 || !x.is_finite() {
        return format!("{x}");
    }
    let p = prec.max(1) as i32;
    let e = format!("{x:.*e}", (p - 1) as usize);
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let trim = |s: &str| -> String {
        if s.contains('.') {
            String::from(s.trim_end_matches('0').trim_end_matches('.'))
        } else {
            String::from(s)
        }
    };
    if exp < -4 || exp >= p {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trim(mant), exp.abs());
    }
    trim(&format!("{x:.*}", (p - 1 - exp) as usize))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "units [-v] [file]");
    let mut verbose = false;
    while let Some(o) = arg.opt() {
        match o {
            'v' => verbose = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let files = arg.argv();
    let file = match files.as_slice() {
        [] => "/lib/units",
        [f] => f.as_str(),
        _ => return Err(usage),
    };
    let text = std::fs::read_to_string(file)
        .map_err(|_| Error(format!("cannot open {file}: {}", purgatorio::errstr())))?;
    let mut db = Db::load(&text);
    let mut out = io::stdout().lock();
    if verbose {
        for name in db.order.clone() {
            let def = db.defs[&name].clone();
            match db.eval_def(&name, &def) {
                Ok(q) => writeln!(out, "{name}\t{def}\t{}", db.show(&q))?,
                Err(e) => writeln!(out, "{name}\t{def}\t{e}")?,
            }
        }
        return Ok(());
    }
    let mut input = io::stdin().lock();
    let mut read = |prompt: &str, out: &mut dyn Write| -> io::Result<Option<String>> {
        write!(out, "{prompt}")?;
        out.flush()?;
        let mut l = String::new();
        if input.read_line(&mut l)? == 0 {
            return Ok(None);
        }
        Ok(Some(String::from(l.trim())))
    };
    'have: while let Some(have) = read("you have: ", &mut out)? {
        if have.is_empty() {
            continue;
        }
        let h = match db.parse(&have) {
            Ok(q) => q,
            Err(e) => {
                writeln!(out, "{e}")?;
                continue;
            }
        };
        loop {
            let Some(want) = read("you want: ", &mut out)? else {
                break 'have;
            };
            if want.is_empty() {
                writeln!(out, "\t{}", db.show(&h))?;
                break;
            }
            let w = match db.parse(&want) {
                Ok(q) => q,
                Err(e) => {
                    writeln!(out, "{e}")?;
                    continue;
                }
            };
            if h.dims != w.dims {
                writeln!(out, "conformability")?;
                writeln!(out, "\t{}", db.show(&h))?;
                writeln!(out, "\t{}", db.show(&w))?;
                break;
            }
            let f = h.f / w.f;
            writeln!(out, "\t* {}", g(f, 7))?;
            writeln!(out, "\t/ {}", g(1.0 / f, 7))?;
            break;
        }
    }
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let mut db = Db::load(&std::fs::read_to_string("../../../root/lib/units").unwrap());
        let mut f = |a: &str, b: &str| {
            let (x, y) = (db.parse(a).unwrap(), db.parse(b).unwrap());
            assert_eq!(x.dims, y.dims, "{a} {b}");
            g(x.f / y.f, 7)
        };
        assert_eq!(f("inch", "cm"), "2.54");
        assert_eq!(f("15 pounds force/in²", "atm"), "1.020689");
        assert_eq!(f("mile", "km"), "1.609344");
        assert_eq!(f("2 hours", "minutes"), "120");
        assert_eq!(f("ms", "s"), "0.001");
        assert_eq!(f("1|2 kg", "gram"), "500");
        assert_eq!(f("lightyear", "parsec"), "0.3066014");
        assert_eq!(g(1.0e-7, 7), "1e-07");
    }
}
