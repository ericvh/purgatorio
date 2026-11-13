//! Addresses written to a window's `addr` file, after acme(4) and the
//! address part of sam(1): `#n` (rune offset), `n` (line), `0`, `$`,
//! `.` (the current address), `/re/` and `?re?` (search forward or back,
//! wrapping round), each optionally followed by `+` or `-` and another
//! of them (relative: `+n` lines after, `-#n` runes before), joined by
//! `,` (from the start of the first to the end of the second) or `;`
//! (the second evaluated from the first). A missing left side of `,` is
//! line 0, a missing right side `$`.

use regexp::Regex;

/// A range of runes, `q0 <= q1`.
pub type Range = (usize, usize);

pub const EBADADDR: &str = "bad address syntax";
pub const ERANGE: &str = "address out of range";

/// Evaluates address `s` in `text`, with `dot` as `.`.
pub fn eval(text: &[char], dot: Range, s: &str) -> Result<Range, &'static str> {
    let mut p = Parser {
        t: text,
        s: s.trim_end_matches('\n').chars().collect(),
        i: 0,
    };
    let r = p.compound(dot)?;
    p.blanks();
    if p.i != p.s.len() {
        return Err(EBADADDR);
    }
    r.ok_or(EBADADDR)
}

struct Parser<'a> {
    t: &'a [char],
    s: Vec<char>,
    i: usize,
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

    fn compound(&mut self, dot: Range) -> Result<Option<Range>, &'static str> {
        let mut left = self.simple(dot)?;
        loop {
            self.blanks();
            let Some(op) = self.peek().filter(|c| *c == ',' || *c == ';') else {
                return Ok(left);
            };
            self.i += 1;
            let l = left.unwrap_or((0, 0));
            let r = self
                .simple(if op == ';' { l } else { dot })?
                .unwrap_or((self.t.len(), self.t.len()));
            if l.0 > r.1 {
                return Err(ERANGE);
            }
            left = Some((l.0, r.1));
        }
    }

    fn simple(&mut self, dot: Range) -> Result<Option<Range>, &'static str> {
        self.blanks();
        let mut a = self.primary(dot, None)?;
        loop {
            self.blanks();
            let Some(sign) = self.peek().filter(|c| *c == '+' || *c == '-') else {
                return Ok(a);
            };
            self.i += 1;
            let base = a.unwrap_or(dot);
            let fwd = sign == '+';
            a = Some(match self.primary(base, Some(fwd))? {
                Some(r) => r,
                // A bare sign is one line.
                None => line(self.t, base, 1, if fwd { 1 } else { -1 })?,
            });
        }
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let s: String = self.s[start..self.i].iter().collect();
        s.parse().ok()
    }

    /// The text up to the closing `delim`, with `\delim` meaning `delim`.
    fn pattern(&mut self, delim: char) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.i += 1;
            if c == delim {
                break;
            }
            if c == '\\' && self.peek() == Some(delim) {
                out.push(delim);
                self.i += 1;
                continue;
            }
            out.push(c);
        }
        out
    }

    /// One address. With `rel`, it is relative to `base`, after it
    /// (`Some(true)`) or before it; otherwise absolute, `base` being dot.
    fn primary(&mut self, base: Range, rel: Option<bool>) -> Result<Option<Range>, &'static str> {
        let n = self.t.len();
        let Some(c) = self.peek() else {
            return Ok(None);
        };
        let r = match c {
            '0'..='9' => {
                let k = self.number().ok_or(EBADADDR)?;
                match rel {
                    None => line(self.t, (0, 0), k, 0)?,
                    Some(true) => line(self.t, base, k, 1)?,
                    Some(false) => line(self.t, base, k, -1)?,
                }
            }
            '#' => {
                self.i += 1;
                let k = self.number().ok_or(EBADADDR)?;
                let q = match rel {
                    None => Some(k),
                    Some(true) => base.1.checked_add(k),
                    Some(false) => base.0.checked_sub(k),
                };
                match q {
                    Some(q) if q <= n => (q, q),
                    _ => return Err(ERANGE),
                }
            }
            '$' if rel.is_none() => {
                self.i += 1;
                (n, n)
            }
            '.' if rel.is_none() => {
                self.i += 1;
                base
            }
            '/' | '?' => {
                self.i += 1;
                let pat = self.pattern(c);
                // `?` searches back; a `-` before either turns it round.
                let fwd = (c == '/') == rel.unwrap_or(true);
                search(self.t, base, &pat, fwd)?
            }
            _ => return Ok(None),
        };
        Ok(Some(r))
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
fn search(t: &[char], base: Range, pat: &str, fwd: bool) -> Result<Range, &'static str> {
    let re = Regex::new(pat).map_err(|_| EBADADDR)?;
    let s: String = t.iter().collect();
    // The byte offset of each rune, and of the end.
    let mut at: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    at.push(s.len());
    let rune = |b: usize| at.partition_point(|&x| x < b);
    let mut all = Vec::new();
    let mut from = 0;
    while from <= s.len() {
        let Some(m) = re
            .find_at(&s, from)
            .and_then(|c| c.first().copied().flatten())
        else {
            break;
        };
        let r = (rune(m.0), rune(m.1));
        all.push(r);
        // Step past the match, or one rune past an empty one.
        from = if m.1 > m.0 {
            m.1
        } else {
            match s[m.1..].chars().next() {
                Some(c) => m.1 + c.len_utf8(),
                None => break,
            }
        };
    }
    let found = if fwd {
        all.iter()
            .find(|r| r.0 >= base.1 && *r != &base)
            .or_else(|| all.first())
    } else {
        all.iter()
            .rev()
            .find(|r| r.1 <= base.0 && *r != &base)
            .or_else(|| all.last())
    };
    found.copied().ok_or(ERANGE)
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
