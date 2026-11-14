//! Addresses, after the address part of sam(1), as acme(4)'s `addr` file
//! and `Edit` take them: `#n` (rune offset), `n` (line), `0`, `$`, `.`
//! (dot), `'` (the mark), `/re/` and `?re?` (search forward or back,
//! wrapping round), each optionally followed by `+` or `-` and another
//! of them (relative: `+n` lines after, `-#n` runes before), joined by
//! `,` (from the start of the first to the end of the second) or `;`
//! (the second evaluated from the first). A missing left side of `,` is
//! line 0, a missing right side `$`; a missing left side of `+` or `-`
//! is dot, a missing right side one line.

use regexp::Regex;

/// A range of runes, `q0 <= q1`.
pub type Range = (usize, usize);

pub const EBADADDR: &str = "bad address syntax";
pub const ERANGE: &str = "address out of range";

/// A parsed address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Addr {
    Char(usize),
    Line(usize),
    End,
    Dot,
    Mark,
    /// A regular expression, searched for forward (`/`) or back (`?`).
    Re(String, bool),
    /// `a+b` (true) or `a-b`: `b` relative to `a`.
    Rel(Option<Box<Addr>>, bool, Option<Box<Addr>>),
    /// `a,b` (false) or `a;b` (true).
    Range(Option<Box<Addr>>, bool, Option<Box<Addr>>),
}

/// The text addresses are evaluated in: its runes, and the same as a
/// string with the byte offset of each rune, for the regexp engine.
pub struct Hay {
    pub t: Vec<char>,
    s: String,
    at: Vec<usize>,
}

/// The rune ranges of a match: the whole match and each subexpression.
pub type Caps = Vec<Option<Range>>;

impl Hay {
    pub fn new(t: Vec<char>) -> Hay {
        let s: String = t.iter().collect();
        let mut at: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
        at.push(s.len());
        Hay { t, s, at }
    }

    pub fn len(&self) -> usize {
        self.t.len()
    }

    fn rune(&self, b: usize) -> usize {
        self.at.partition_point(|&x| x < b)
    }

    /// The first match of `re` starting at or after rune `from`, in the
    /// text up to rune `end` (which then counts as the end of the text).
    pub fn find(&self, re: &Regex, from: usize, end: usize) -> Option<Caps> {
        let end = end.min(self.len());
        if from > end {
            return None;
        }
        let s = &self.s[..self.at[end]];
        let caps = re.find_at(s, self.at[from])?;
        Some(
            caps.into_iter()
                .map(|c| c.map(|(a, b)| (self.rune(a), self.rune(b))))
                .collect(),
        )
    }

    /// The whole of the first match of `re` at or after `from`.
    pub fn find0(&self, re: &Regex, from: usize, end: usize) -> Option<Range> {
        self.find(re, from, end)
            .and_then(|c| c.first().copied().flatten())
    }

    pub fn string(&self, q0: usize, q1: usize) -> String {
        let n = self.len();
        self.t[q0.min(n)..q1.min(n)].iter().collect()
    }
}

/// Evaluates address `s` in `text`, with `dot` as `.`.
pub fn eval(text: &[char], dot: Range, s: &str) -> Result<Range, &'static str> {
    let cs: Vec<char> = s.trim_end_matches('\n').chars().collect();
    let mut i = 0;
    let a = parse(&cs, &mut i)?;
    blanks(&cs, &mut i);
    if i != cs.len() {
        return Err(EBADADDR);
    }
    let a = a.ok_or(EBADADDR)?;
    let h = Hay::new(text.to_vec());
    resolve(&h, dot, None, &a)
}

fn blanks(s: &[char], i: &mut usize) {
    while s.get(*i).is_some_and(|c| *c == ' ' || *c == '\t') {
        *i += 1;
    }
}

/// Parses an address from `s` at `*i`, leaving `*i` after it; `None` when
/// there is none there.
pub fn parse(s: &[char], i: &mut usize) -> Result<Option<Addr>, &'static str> {
    let mut left = simple(s, i)?;
    loop {
        blanks(s, i);
        let Some(op) = s.get(*i).copied().filter(|c| *c == ',' || *c == ';') else {
            return Ok(left);
        };
        *i += 1;
        let right = simple(s, i)?;
        left = Some(Addr::Range(
            left.map(Box::new),
            op == ';',
            right.map(Box::new),
        ));
    }
}

fn simple(s: &[char], i: &mut usize) -> Result<Option<Addr>, &'static str> {
    blanks(s, i);
    let mut a = primary(s, i)?;
    loop {
        blanks(s, i);
        let Some(sign) = s.get(*i).copied().filter(|c| *c == '+' || *c == '-') else {
            return Ok(a);
        };
        *i += 1;
        blanks(s, i);
        let b = primary(s, i)?;
        a = Some(Addr::Rel(a.map(Box::new), sign == '+', b.map(Box::new)));
    }
}

fn number(s: &[char], i: &mut usize) -> Option<usize> {
    let start = *i;
    while s.get(*i).is_some_and(|c| c.is_ascii_digit()) {
        *i += 1;
    }
    let t: String = s[start..*i].iter().collect();
    t.parse().ok()
}

/// The text up to the closing `delim` (or the end of the line), with
/// `\delim` meaning `delim`; other escapes are kept for the regexp.
pub fn pattern(s: &[char], i: &mut usize, delim: char) -> String {
    let mut out = String::new();
    while let Some(&c) = s.get(*i) {
        if c == '\n' {
            break;
        }
        *i += 1;
        if c == delim {
            break;
        }
        if c == '\\' && s.get(*i) == Some(&delim) {
            out.push(delim);
            *i += 1;
            continue;
        }
        if c == '\\'
            && let Some(&d) = s.get(*i)
        {
            out.push(c);
            out.push(d);
            *i += 1;
            continue;
        }
        out.push(c);
    }
    out
}

fn primary(s: &[char], i: &mut usize) -> Result<Option<Addr>, &'static str> {
    let Some(&c) = s.get(*i) else {
        return Ok(None);
    };
    let a = match c {
        '0'..='9' => Addr::Line(number(s, i).ok_or(EBADADDR)?),
        '#' => {
            *i += 1;
            Addr::Char(number(s, i).ok_or(EBADADDR)?)
        }
        '$' => {
            *i += 1;
            Addr::End
        }
        '.' => {
            *i += 1;
            Addr::Dot
        }
        '\'' => {
            *i += 1;
            Addr::Mark
        }
        '/' | '?' => {
            *i += 1;
            Addr::Re(pattern(s, i, c), c == '/')
        }
        _ => return Ok(None),
    };
    Ok(Some(a))
}

/// Evaluates `a` in `h` with `dot` as `.` and `mark` as `'`.
pub fn resolve(h: &Hay, dot: Range, mark: Option<Range>, a: &Addr) -> Result<Range, &'static str> {
    let n = h.len();
    Ok(match a {
        Addr::Char(k) if *k <= n => (*k, *k),
        Addr::Char(_) => return Err(ERANGE),
        Addr::Line(k) => line(&h.t, (0, 0), *k, 0)?,
        Addr::End => (n, n),
        Addr::Dot => dot,
        Addr::Mark => mark.unwrap_or((0, 0)),
        Addr::Re(p, fwd) => search(h, dot, p, *fwd)?,
        Addr::Rel(l, fwd, r) => {
            let base = match l {
                Some(l) => resolve(h, dot, mark, l)?,
                None => dot,
            };
            relative(h, base, mark, *fwd, r.as_deref())?
        }
        Addr::Range(l, semi, r) => {
            let l = match l {
                Some(l) => resolve(h, dot, mark, l)?,
                None => (0, 0),
            };
            let r = match r {
                Some(r) => resolve(h, if *semi { l } else { dot }, mark, r)?,
                None => (n, n),
            };
            if l.0 > r.1 {
                return Err(ERANGE);
            }
            (l.0, r.1)
        }
    })
}

/// `b` relative to `base`, after it (`fwd`) or before it.
fn relative(
    h: &Hay,
    base: Range,
    mark: Option<Range>,
    fwd: bool,
    b: Option<&Addr>,
) -> Result<Range, &'static str> {
    let n = h.len();
    let sign = if fwd { 1 } else { -1 };
    match b {
        None => line(&h.t, base, 1, sign),
        Some(Addr::Line(k)) => line(&h.t, base, *k, sign),
        Some(Addr::Char(k)) => {
            let q = if fwd {
                base.1.checked_add(*k)
            } else {
                base.0.checked_sub(*k)
            };
            match q {
                Some(q) if q <= n => Ok((q, q)),
                _ => Err(ERANGE),
            }
        }
        // A `-` turns a search round.
        Some(Addr::Re(p, f)) => search(h, base, p, *f == fwd),
        Some(other) => resolve(h, base, mark, other),
    }
}

/// Line `k` counted from the start (`sign` 0), or `k` lines after the end
/// of `base` (1) or before its start (-1), as sam counts them: line 0 is
/// the empty string at the start, and the line after the last newline
/// is the empty string at the end.
pub fn line(t: &[char], base: Range, k: usize, sign: i32) -> Result<Range, &'static str> {
    let n = t.len();
    if sign >= 0 {
        let mut p;
        let q0;
        if k == 0 {
            if sign == 0 || base.1 == 0 {
                return Ok((0, 0));
            }
            q0 = base.1;
            p = base.1 - 1;
        } else {
            let mut seen;
            if sign == 0 || base.1 == 0 {
                p = 0;
                seen = 1;
            } else {
                p = base.1 - 1;
                seen = usize::from(t[p] == '\n');
                p += 1;
            }
            while seen < k {
                if p >= n {
                    return Err(ERANGE);
                }
                if t[p] == '\n' {
                    seen += 1;
                }
                p += 1;
            }
            q0 = p;
        }
        while p < n {
            let c = t[p];
            p += 1;
            if c == '\n' {
                break;
            }
        }
        return Ok((q0, p));
    }
    let mut p = base.0;
    let q1;
    if k == 0 {
        q1 = base.0;
    } else {
        let mut seen = 0;
        while seen < k {
            if p == 0 {
                seen += 1;
                if seen != k {
                    return Err(ERANGE);
                }
            } else if t[p - 1] != '\n' {
                p -= 1;
            } else {
                seen += 1;
                if seen != k {
                    p -= 1;
                }
            }
        }
        q1 = p;
        p = p.saturating_sub(1);
    }
    while p > 0 && t[p - 1] != '\n' {
        p -= 1;
    }
    Ok((p, q1))
}

/// The next match of `pat` after `base` (or the last before it), wrapping
/// round the text.
pub fn search(h: &Hay, base: Range, pat: &str, fwd: bool) -> Result<Range, &'static str> {
    let re = Regex::new(pat).map_err(|_| EBADADDR)?;
    search_re(h, base, &re, fwd)
}

/// [`search`] with a compiled expression.
pub fn search_re(h: &Hay, base: Range, re: &Regex, fwd: bool) -> Result<Range, &'static str> {
    let n = h.len();
    if fwd {
        let mut m = h.find0(re, base.1, n);
        // A null match where the search started is no progress.
        if m == Some(base) && base.0 == base.1 {
            m = if base.1 < n {
                h.find0(re, base.1 + 1, n)
            } else {
                None
            };
        }
        return m.or_else(|| h.find0(re, 0, n)).ok_or(ERANGE);
    }
    // Back: the last match that ends by the start of base, else the last
    // of all.
    let mut before = None;
    let mut last = None;
    let mut from = 0;
    while let Some(m) = h.find0(re, from, n) {
        if m.1 <= base.0 && m != base {
            before = Some(m);
        }
        last = Some(m);
        from = if m.1 > m.0 { m.1 } else { m.1 + 1 };
        if from > n {
            break;
        }
    }
    before.or(last).ok_or(ERANGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: &str, dot: Range, s: &str) -> Result<Range, &'static str> {
        let t: Vec<char> = t.chars().collect();
        eval(&t, dot, s)
    }

    const T: &str = "one\ntwo\nthree\n";

    #[test]
    fn simple() {
        assert_eq!(ev(T, (0, 0), "1"), Ok((0, 4)));
        assert_eq!(ev(T, (0, 0), "2"), Ok((4, 8)));
        assert_eq!(ev(T, (0, 0), "3"), Ok((8, 14)));
        assert_eq!(ev(T, (0, 0), "4"), Ok((14, 14)));
        assert_eq!(ev(T, (0, 0), "5"), Err(ERANGE));
        assert_eq!(ev(T, (5, 6), "0"), Ok((0, 0)));
        assert_eq!(ev(T, (0, 0), "$"), Ok((14, 14)));
        assert_eq!(ev(T, (0, 0), "#3"), Ok((3, 3)));
        assert_eq!(ev(T, (0, 0), "#15"), Err(ERANGE));
        assert_eq!(ev(T, (5, 6), "."), Ok((5, 6)));
        assert_eq!(ev(T, (0, 0), ""), Err(EBADADDR));
        assert_eq!(ev(T, (0, 0), "x"), Err(EBADADDR));
        assert_eq!(ev(T, (0, 0), "#"), Err(EBADADDR));
        // A last line without a newline is still a line.
        assert_eq!(ev("a\nb", (0, 0), "2"), Ok((2, 3)));
    }

    #[test]
    fn compound() {
        assert_eq!(ev(T, (0, 0), "#3,#5"), Ok((3, 5)));
        assert_eq!(ev(T, (0, 0), "2,3"), Ok((4, 14)));
        assert_eq!(ev(T, (0, 0), ","), Ok((0, 14)));
        assert_eq!(ev(T, (0, 0), "2,"), Ok((4, 14)));
        assert_eq!(ev(T, (0, 0), ",2"), Ok((0, 8)));
        assert_eq!(ev(T, (0, 0), "#5,#3"), Err(ERANGE));
        assert_eq!(ev(T, (0, 0), "2;+1"), Ok((4, 14)));
        assert_eq!(ev(T, (0, 0), "0,$\n"), Ok((0, 14)));
    }

    #[test]
    fn relative() {
        assert_eq!(ev(T, (0, 0), "2+1"), Ok((8, 14)));
        assert_eq!(ev(T, (0, 0), "3-1"), Ok((4, 8)));
        assert_eq!(ev(T, (0, 0), "2+"), Ok((8, 14)));
        assert_eq!(ev(T, (0, 0), "$-#2"), Ok((12, 12)));
        assert_eq!(ev(T, (0, 0), "#1+#2"), Ok((3, 3)));
        assert_eq!(ev(T, (0, 0), "#1-#2"), Err(ERANGE));
        assert_eq!(ev(T, (4, 8), ".+1"), Ok((8, 14)));
        assert_eq!(ev(T, (0, 0), "1-1"), Ok((0, 0)));
    }

    #[test]
    fn regexps() {
        assert_eq!(ev(T, (0, 0), "/two/"), Ok((4, 7)));
        assert_eq!(ev(T, (0, 0), "/t.*/"), Ok((4, 7)));
        // Forward from the end of dot, wrapping round.
        assert_eq!(ev(T, (9, 9), "/e/"), Ok((11, 12)));
        assert_eq!(ev(T, (13, 13), "/o/"), Ok((0, 1)));
        assert_eq!(ev(T, (14, 14), "?o?"), Ok((6, 7)));
        assert_eq!(ev(T, (0, 0), "?o?"), Ok((6, 7)));
        assert_eq!(ev(T, (0, 0), "2-/one/"), Ok((0, 3)));
        assert_eq!(ev(T, (0, 0), "/zz/"), Err(ERANGE));
        assert_eq!(ev(T, (0, 0), "/(/"), Err(EBADADDR));
        assert_eq!(ev("a/b", (0, 0), "/\\//"), Ok((1, 2)));
        assert_eq!(ev("héllo", (0, 0), "/l+/"), Ok((2, 4)));
    }
}
