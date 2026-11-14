//! Laying text out in lines: where each visible line starts and ends,
//! with long lines wrapped at the frame's width and tabs at multiples of
//! the tab width, as libframe lays out acme's text.

use crate::text::Buffer;

/// How wide runes are.
pub trait Widths {
    /// The width of `c` in pixels.
    fn width(&mut self, c: char) -> i32;
    /// The distance between tab stops.
    fn tab(&mut self) -> i32;
}

/// One visible line: runes `q0..q1`, without the newline that ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub q0: usize,
    pub q1: usize,
    /// Where the next line starts: after the newline, or at `q1` for a
    /// wrapped line.
    pub next: usize,
}

/// The width of `c` drawn at `x` (a tab reaches the next stop).
pub fn advance(w: &mut dyn Widths, c: char, x: i32) -> i32 {
    if c == '\t' {
        let t = w.tab().max(1);
        t - x % t
    } else {
        w.width(c)
    }
}

/// Lays out up to `max` lines of `b` from `org` in `width` pixels. A
/// text that ends in a newline gets an empty last line, so the cursor
/// can be shown there.
pub fn layout(b: &Buffer, org: usize, width: i32, max: usize, w: &mut dyn Widths) -> Vec<Line> {
    let len = b.len();
    let mut lines = Vec::new();
    let mut q = org.min(len);
    while lines.len() < max {
        let start = q;
        let mut x = 0;
        let (end, next, nl) = loop {
            if q >= len {
                break (q, q, false);
            }
            let c = b.at(q);
            if c == '\n' {
                break (q, q + 1, true);
            }
            let a = advance(w, c, x);
            if x + a > width && q > start {
                break (q, q, false);
            }
            x += a;
            q += 1;
        };
        lines.push(Line {
            q0: start,
            q1: end,
            next,
        });
        if !nl && next >= len {
            break;
        }
        q = next;
    }
    lines
}

/// The start of the logical line (after a newline) that holds `q`.
pub fn line_start(b: &Buffer, q: usize) -> usize {
    let mut s = q.min(b.len());
    while s > 0 && b.at(s - 1) != '\n' {
        s -= 1;
    }
    s
}

/// The starts of the wrapped lines from `s` (a logical line's start)
/// that begin before `org`.
fn starts_before(b: &Buffer, s: usize, org: usize, width: i32, w: &mut dyn Widths) -> Vec<usize> {
    let mut out = vec![s];
    let mut q = s;
    loop {
        let l = layout(b, q, width, 1, w);
        let Some(l) = l.first() else { break };
        if l.next >= org || l.next == q || l.next > l.q1 || l.next >= b.len() {
            break;
        }
        q = l.next;
        out.push(q);
    }
    out
}

/// The start of the wrapped line that holds `q`.
pub fn wrapped_start(b: &Buffer, q: usize, width: i32, w: &mut dyn Widths) -> usize {
    let s = line_start(b, q);
    let starts = starts_before(b, s, q + 1, width, w);
    *starts.last().unwrap_or(&s)
}

/// The start of the line `n` lines above the one starting at `org`.
pub fn back(b: &Buffer, mut org: usize, mut n: usize, width: i32, w: &mut dyn Widths) -> usize {
    while n > 0 && org > 0 {
        let s = line_start(b, org - 1);
        let starts = starts_before(b, s, org, width, w);
        if starts.len() >= n {
            return starts[starts.len() - n];
        }
        n -= starts.len();
        org = s;
    }
    org
}

/// The x offset of `q` within `line`.
pub fn x_of(b: &Buffer, line: &Line, q: usize, w: &mut dyn Widths) -> i32 {
    let mut x = 0;
    for p in line.q0..q.min(line.q1) {
        x += advance(w, b.at(p), x);
    }
    x
}

/// The rune nearest to x offset `x` in `line`.
pub fn q_at(b: &Buffer, line: &Line, x: i32, w: &mut dyn Widths) -> usize {
    let mut cx = 0;
    for p in line.q0..line.q1 {
        let a = advance(w, b.at(p), cx);
        if x < cx + a / 2 + a % 2 {
            return p;
        }
        cx += a;
    }
    line.q1
}

/// Which of `lines` shows `q`, if any.
pub fn line_of(lines: &[Line], q: usize) -> Option<usize> {
    let last = lines.len().checked_sub(1)?;
    lines
        .iter()
        .enumerate()
        .position(|(i, l)| q >= l.q0 && (q < l.next || (q == l.q1 && (i == last || l.next > l.q1))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rune 10 pixels, tabs every 40.
    struct Fixed;

    impl Widths for Fixed {
        fn width(&mut self, _c: char) -> i32 {
            10
        }
        fn tab(&mut self) -> i32 {
            40
        }
    }

    fn buf(s: &str) -> Buffer {
        let b = Buffer::new(false);
        b.insert(0, s);
        b
    }

    #[test]
    fn wraps_and_newlines() {
        let b = buf("abcdefgh\nxy\n");
        let l = layout(&b, 0, 50, 10, &mut Fixed);
        let r: Vec<(usize, usize, usize)> = l.iter().map(|l| (l.q0, l.q1, l.next)).collect();
        assert_eq!(r, [(0, 5, 5), (5, 8, 9), (9, 11, 12), (12, 12, 12)]);
        assert_eq!(layout(&b, 0, 50, 2, &mut Fixed).len(), 2);
        assert_eq!(layout(&buf(""), 0, 50, 5, &mut Fixed).len(), 1);
        assert_eq!(line_of(&l, 5), Some(1));
        assert_eq!(line_of(&l, 8), Some(1));
        assert_eq!(line_of(&l, 12), Some(3));
    }

    #[test]
    fn tabs() {
        let b = buf("a\tb");
        let l = layout(&b, 0, 200, 1, &mut Fixed);
        assert_eq!(x_of(&b, &l[0], 2, &mut Fixed), 40);
        assert_eq!(q_at(&b, &l[0], 41, &mut Fixed), 2);
        assert_eq!(q_at(&b, &l[0], 14, &mut Fixed), 1);
        assert_eq!(q_at(&b, &l[0], 500, &mut Fixed), 3);
    }

    #[test]
    fn backwards() {
        let b = buf("abcdefgh\nxy\nz");
        assert_eq!(back(&b, 12, 1, 50, &mut Fixed), 9);
        assert_eq!(back(&b, 12, 2, 50, &mut Fixed), 5);
        assert_eq!(back(&b, 12, 3, 50, &mut Fixed), 0);
        assert_eq!(back(&b, 12, 9, 50, &mut Fixed), 0);
        assert_eq!(wrapped_start(&b, 7, 50, &mut Fixed), 5);
        assert_eq!(wrapped_start(&b, 3, 50, &mut Fixed), 0);
        assert_eq!(wrapped_start(&b, 13, 50, &mut Fixed), 12);
    }
}
