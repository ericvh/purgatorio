//! A text window: what a shell in it has written, and what has been
//! typed for it to read.
//!
//! The text is one run of runes. The output point `qh` divides it, as in
//! Plan 9's rio: before it is what was written to `/dev/cons` and what
//! programs have read; after it is typing nobody has read yet. A write
//! goes in at `qh`, before any such typing, and a read takes runes from
//! `qh` up to `ready`: the end of the last line ended by a newline, or the
//! place where `^D` was typed. `^D` with nothing waiting makes the next
//! read return nothing, an end of file. Typing after `ready` can be
//! erased.
//!
//! The selection `q0..q1` is where typing goes, as in rio: a rune typed
//! replaces the selection and the selection becomes the point after it.
//! When the selection is before the text that may still be changed,
//! typing goes at the end instead. Cut and paste work anywhere.

use std::collections::HashMap;

use purgatorio::draw::{Font, Point, Rect};

/// Runes kept before the oldest are dropped.
const MAXTEXT: usize = 256 * 1024;
/// Runes dropped at a time when there are too many.
const TRIM: usize = 64 * 1024;
/// Tab stops are this many widths of `0` apart.
const TABSTOP: i32 = 4;

#[derive(Default)]
pub struct Text {
    pub s: Vec<char>,
    /// The output point.
    pub qh: usize,
    /// Typing before this may be read.
    pub ready: usize,
    /// Ends of file typed and not yet read.
    pub eof: u32,
    /// The first rune shown.
    pub org: usize,
    /// The selection.
    pub q0: usize,
    pub q1: usize,
}

/// Whether `c` is part of a word, for a double click.
fn wordch(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Rune widths in the window's font, remembered.
pub struct Widths {
    pub font: Font,
    known: HashMap<char, i32>,
    tab: i32,
}

impl Widths {
    pub fn new(mut font: Font) -> Widths {
        let tab = TABSTOP * font.string_width("0").max(1);
        Widths {
            font,
            known: HashMap::new(),
            tab,
        }
    }

    pub fn of(&mut self, c: char) -> i32 {
        if let Some(&w) = self.known.get(&c) {
            return w;
        }
        let w = self.font.string_width(c.encode_utf8(&mut [0u8; 4]));
        self.known.insert(c, w);
        w
    }

    /// How far rune `c` moves a point at `x`.
    pub fn advance(&mut self, c: char, x: i32) -> i32 {
        if c == '\t' {
            self.tab - x % self.tab
        } else {
            self.of(c)
        }
    }
}

impl Text {
    /// The first rune that typing may still change.
    fn fixed(&self) -> usize {
        self.qh.max(self.ready)
    }

    /// Inserts `add` at `q`. The output point, `ready`, the origin and
    /// the selection move when they are after `q`.
    pub fn insert(&mut self, q: usize, add: &[char]) {
        let n = add.len();
        let q = q.min(self.s.len());
        self.s.splice(q..q, add.iter().copied());
        for m in [
            &mut self.qh,
            &mut self.ready,
            &mut self.org,
            &mut self.q0,
            &mut self.q1,
        ] {
            if *m > q {
                *m += n;
            }
        }
    }

    /// Deletes `q0..q1`; marks inside it go to its start.
    pub fn delete(&mut self, q0: usize, q1: usize) {
        let q1 = q1.min(self.s.len());
        if q0 >= q1 {
            return;
        }
        let n = q1 - q0;
        self.s.drain(q0..q1);
        for m in [
            &mut self.qh,
            &mut self.ready,
            &mut self.org,
            &mut self.q0,
            &mut self.q1,
        ] {
            if *m >= q1 {
                *m -= n;
            } else if *m > q0 {
                *m = q0;
            }
        }
    }

    /// The selected text.
    pub fn selected(&self) -> String {
        let n = self.s.len();
        self.s[self.q0.min(n)..self.q1.min(n)].iter().collect()
    }

    /// Selects from `a` to `b`, either way round.
    pub fn select(&mut self, a: usize, b: usize) {
        let n = self.s.len();
        self.q0 = a.min(b).min(n);
        self.q1 = a.max(b).min(n);
    }

    /// What a double click at `q` selects: the line when `q` is at its
    /// start or its end, else the word around `q`.
    pub fn double(&self, q: usize) -> (usize, usize) {
        let q = q.min(self.s.len());
        let ls = self.line_start(q);
        let le = self.line_end(q);
        if q == ls || q == le {
            return (ls, (le + 1).min(self.s.len()));
        }
        let mut a = q;
        while a > ls && wordch(self.s[a - 1]) {
            a -= 1;
        }
        let mut b = q;
        while b < le && wordch(self.s[b]) {
            b += 1;
        }
        (a, b)
    }

    /// Output from a program, inserted at the output point, before any
    /// typing; a selection at or after the output point moves on.
    pub fn output(&mut self, s: &str) {
        let add: Vec<char> = s.chars().filter(|&c| c != '\0').collect();
        let n = add.len();
        let q = self.qh;
        self.s.splice(q..q, add);
        self.qh += n;
        self.ready += n;
        if self.org > q {
            self.org += n;
        }
        if self.q0 >= q {
            self.q0 += n;
        }
        if self.q1 >= q {
            self.q1 += n;
        }
        self.trim();
    }

    /// Text added at the end as if typed, with a newline if it has none,
    /// and made readable: rio's `send`.
    pub fn send(&mut self, s: &str) {
        let mut add: Vec<char> = s.chars().collect();
        if add.last() != Some(&'\n') {
            add.push('\n');
        }
        let end = self.s.len();
        self.insert(end, &add);
        self.ready = self.s.len();
        self.q0 = self.s.len();
        self.q1 = self.q0;
        self.trim();
    }

    /// `s` in place of the selection, selected: rio's `paste`. Pasted
    /// into what is typed, it may be read up to its last newline, as if
    /// typed.
    pub fn paste(&mut self, s: &str) {
        let add: Vec<char> = s.chars().collect();
        self.delete(self.q0, self.q1);
        let q = self.q0;
        let typing = q >= self.fixed();
        self.insert(q, &add);
        if typing && let Some(nl) = add.iter().rposition(|&c| c == '\n') {
            self.ready = self.ready.max(q + nl + 1);
        }
        self.q0 = q;
        self.q1 = q + add.len();
        self.trim();
    }

    /// A rune typed at the selection, or at the end when the selection
    /// is in text typing may not change.
    pub fn typed(&mut self, c: char) {
        let floor = self.fixed();
        if self.q0 < floor {
            self.q0 = self.s.len();
            self.q1 = self.q0;
        }
        let erase = matches!(c, '\x08' | '\x15' | '\x17');
        if self.q1 > self.q0 && c != '\x04' {
            self.delete(self.q0, self.q1);
            if erase {
                return;
            }
        }
        let q = self.q0;
        match c {
            '\x08' => {
                if q > floor {
                    self.delete(q - 1, q);
                }
            }
            // ^U: back to the start of the line or what can be erased.
            '\x15' => {
                let a = self.line_start(q).max(floor);
                self.delete(a, q);
            }
            // ^W: the word before.
            '\x17' => {
                let mut a = q;
                while a > floor && self.s[a - 1].is_whitespace() {
                    a -= 1;
                }
                while a > floor && !self.s[a - 1].is_whitespace() {
                    a -= 1;
                }
                self.delete(a, q);
            }
            // ^D: what is typed may be read without a newline; alone, it
            // is an end of file.
            '\x04' => {
                if self.s.len() == self.qh {
                    self.eof += 1;
                }
                self.ready = self.s.len();
            }
            '\n' | '\r' => {
                self.insert(q, &['\n']);
                self.ready = self.ready.max(q + 1);
                self.q0 = q + 1;
                self.q1 = q + 1;
            }
            c => {
                self.insert(q, &[c]);
                self.q0 = q + 1;
                self.q1 = q + 1;
            }
        }
        self.trim();
    }

    /// Whether a read would return now.
    pub fn readable(&self) -> bool {
        self.ready > self.qh || self.eof > 0
    }

    /// A read of at most `count` bytes: whole runes from the output point
    /// up to `ready`, or an end of file. `None` when it must wait.
    pub fn read(&mut self, count: usize) -> Option<Vec<u8>> {
        if self.ready > self.qh {
            let mut out = Vec::new();
            let mut q = self.qh;
            while q < self.ready {
                let mut b = [0u8; 4];
                let e = self.s[q].encode_utf8(&mut b);
                if out.len() + e.len() > count {
                    break;
                }
                out.extend_from_slice(e.as_bytes());
                q += 1;
            }
            if q == self.qh {
                return None;
            }
            self.qh = q;
            return Some(out);
        }
        if self.eof > 0 {
            self.eof -= 1;
            return Some(Vec::new());
        }
        None
    }

    /// Typing nobody has read yet goes: the interrupt key's work.
    pub fn flush_input(&mut self) {
        self.s.truncate(self.qh);
        self.ready = self.qh;
        self.eof = 0;
        self.org = self.org.min(self.s.len());
        self.q0 = self.q0.min(self.s.len());
        self.q1 = self.q1.min(self.s.len());
    }

    /// Drops the oldest runes once there are too many.
    fn trim(&mut self) {
        if self.s.len() <= MAXTEXT {
            return;
        }
        let n = TRIM.min(self.qh);
        self.s.drain(..n);
        self.qh -= n;
        self.ready = self.ready.saturating_sub(n);
        self.org = self.org.saturating_sub(n);
        self.q0 = self.q0.saturating_sub(n);
        self.q1 = self.q1.saturating_sub(n);
    }

    pub fn all(&self) -> String {
        self.s.iter().collect()
    }

    /// The end of the logical line from `ls`: its newline, or the end.
    fn line_end(&self, ls: usize) -> usize {
        self.s[ls..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.s.len(), |i| ls + i)
    }

    /// The start of the logical line holding `q`.
    fn line_start(&self, q: usize) -> usize {
        self.s[..q]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1)
    }

    /// Where the lines a logical line `[ls, le)` wraps into start, at
    /// width `w`.
    fn wraps(&self, ls: usize, le: usize, w: i32, f: &mut Widths) -> Vec<usize> {
        let mut out = vec![ls];
        let mut x = 0;
        for i in ls..le {
            let a = f.advance(self.s[i], x);
            if x + a > w && x > 0 {
                out.push(i);
                x = 0;
            }
            x += f.advance(self.s[i], x);
        }
        out
    }

    /// The lines from `org`, at most `n`: each `(start, end)`, the end
    /// past its newline if it has one.
    pub fn layout(&self, org: usize, n: usize, w: i32, f: &mut Widths) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut q = org;
        while lines.len() < n {
            let le = self.line_end(q);
            let starts = self.wraps(q, le, w, f);
            for (i, &s) in starts.iter().enumerate() {
                let e = starts.get(i + 1).copied().unwrap_or(le);
                lines.push((s, e));
                if lines.len() == n {
                    break;
                }
            }
            if le >= self.s.len() {
                break;
            }
            if let Some(last) = lines.last_mut()
                && last.1 == le
            {
                last.1 = le + 1;
            }
            q = le + 1;
        }
        lines
    }

    /// Line starts before `q`, the nearest last, as far back as `n`.
    fn starts_before(&self, q: usize, n: usize, w: i32, f: &mut Widths) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        let mut end = q;
        loop {
            let ls = self.line_start(end);
            let mut starts = self.wraps(ls, self.line_end(ls).min(end.max(ls)), w, f);
            starts.retain(|&s| s < q || (s == q && q == self.s.len() && ls == q));
            starts.extend(out);
            out = starts;
            if out.len() >= n || ls == 0 {
                return out;
            }
            end = ls - 1;
        }
    }

    /// The first rune to show so that the end shows on the last of `n`
    /// lines.
    pub fn org_for_end(&self, n: usize, w: i32, f: &mut Widths) -> usize {
        let starts = self.starts_before(self.s.len(), n, w, f);
        starts[starts.len().saturating_sub(n.max(1))]
    }

    /// Whether the end shows with the text from `org` on `n` lines.
    pub fn end_shows(&self, n: usize, w: i32, f: &mut Widths) -> bool {
        let lines = self.layout(self.org, n, w, f);
        self.end_point(&lines, 1, f).is_some()
    }

    /// Scrolls by `n` lines, down for positive.
    pub fn scroll(&mut self, n: i32, w: i32, f: &mut Widths) {
        if n > 0 {
            let lines = self.layout(self.org, n as usize + 1, w, f);
            if let Some(&(s, _)) = lines.get(n as usize) {
                self.org = s;
            } else if let Some(&(_, e)) = lines.last() {
                self.org = e.min(self.s.len());
            }
        } else if n < 0 && self.org > 0 {
            let starts = self.starts_before(self.org, (-n) as usize, w, f);
            let k = (-n) as usize;
            self.org = starts[starts.len().saturating_sub(k)];
        }
    }

    /// The runes of a line to draw, as stretches between tabs: each a
    /// string and the x it starts at, from 0.
    pub fn pieces(&self, s: usize, e: usize, f: &mut Widths) -> Vec<(String, i32)> {
        let mut out: Vec<(String, i32)> = Vec::new();
        let mut cur = String::new();
        let (mut x, mut x0) = (0, 0);
        for &c in &self.s[s..e] {
            if c == '\n' {
                break;
            }
            if c == '\t' {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), x0));
                }
                x += f.advance(c, x);
                x0 = x;
                continue;
            }
            cur.push(c);
            x += f.advance(c, x);
        }
        if !cur.is_empty() {
            out.push((cur, x0));
        }
        out
    }

    /// Where the end of the text is drawn, from the top left of the
    /// lines `h` high, when it shows.
    pub fn end_point(&self, lines: &[(usize, usize)], h: i32, f: &mut Widths) -> Option<Point> {
        self.point_of(self.s.len(), lines, h, f)
    }

    /// Where the point before rune `q` is drawn, from the top left of
    /// the lines `h` high, when it shows.
    pub fn point_of(
        &self,
        q: usize,
        lines: &[(usize, usize)],
        h: i32,
        f: &mut Widths,
    ) -> Option<Point> {
        let (i, &(s, _)) = lines
            .iter()
            .enumerate()
            .find(|(_, (s, e))| *s <= q && q < *e)
            .or_else(|| {
                lines
                    .iter()
                    .enumerate()
                    .find(|(_, (s, e))| *e == q && (s == e || self.s[e - 1] != '\n'))
            })?;
        Some(Point::new(self.x_of(s, q, f), i as i32 * h))
    }

    /// How far from the start of the line at `s` rune `q` is drawn.
    pub fn x_of(&self, s: usize, q: usize, f: &mut Widths) -> i32 {
        let mut x = 0;
        for &c in &self.s[s..q.min(self.s.len())] {
            if c == '\n' {
                break;
            }
            x += f.advance(c, x);
        }
        x
    }

    /// The rune at `x` in the line `(s, e)`: the one whose cell holds
    /// `x`, or the line's end (its newline) past the last.
    pub fn at_x(&self, s: usize, e: usize, x: i32, f: &mut Widths) -> usize {
        let mut cx = 0;
        for q in s..e {
            let c = self.s[q];
            if c == '\n' {
                return q;
            }
            let a = f.advance(c, cx);
            if x < cx + a {
                return q;
            }
            cx += a;
        }
        e
    }

    /// The rectangle of the text area of a window `r` drawn with border
    /// `b`: a margin inside the border.
    pub fn area(r: Rect, b: i32) -> Rect {
        Rect::new(
            r.min.x + b + 4,
            r.min.y + b + 2,
            r.max.x - b - 4,
            r.max.y - b - 2,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Text {
        let mut t = Text::default();
        t.output(s);
        t
    }

    #[test]
    fn output_goes_before_typing() {
        let mut t = text("; ");
        for c in "echo hi\n".chars() {
            t.typed(c);
        }
        t.output("x");
        assert_eq!(t.all(), "; xecho hi\n");
        assert_eq!(t.read(100).unwrap(), b"echo hi\n");
        t.output("hi\n; ");
        assert_eq!(t.all(), "; xecho hi\nhi\n; ");
        assert!(t.read(100).is_none());
    }

    #[test]
    fn erasing_and_end_of_file() {
        let mut t = text("; ");
        for c in "ab\x08c\n".chars() {
            t.typed(c);
        }
        // The line is sent: backspace cannot reach it.
        t.typed('\x08');
        assert_eq!(t.all(), "; ac\n");
        for c in "one two\x17\x17x\x15y".chars() {
            t.typed(c);
        }
        assert_eq!(t.all(), "; ac\ny");
        assert_eq!(t.read(1).unwrap(), b"a");
        assert_eq!(t.read(100).unwrap(), b"c\n");
        t.typed('\x04');
        assert_eq!(t.read(100).unwrap(), b"y");
        t.typed('\x04');
        assert_eq!(t.read(100).unwrap(), b"");
        assert!(t.read(100).is_none());
    }

    #[test]
    fn typing_at_the_selection() {
        let mut t = text("; ");
        for c in "echo hi".chars() {
            t.typed(c);
        }
        // Select "hi" and type over it.
        t.select(7, 9);
        assert_eq!(t.selected(), "hi");
        t.typed('x');
        assert_eq!(t.all(), "; echo x");
        // Output goes before the typing and the point moves with it.
        t.output("out\n");
        assert_eq!(t.all(), "; out\necho x");
        assert_eq!((t.q0, t.q1), (12, 12));
        // A selection in the output: typing goes at the end.
        t.select(2, 5);
        t.typed('!');
        assert_eq!(t.all(), "; out\necho x!");
        t.typed('\n');
        assert_eq!(t.read(100).unwrap(), b"echo x!\n");
    }

    #[test]
    fn cut_paste_and_send() {
        let mut t = text("; hello\n; ");
        t.select(2, 7);
        let s = t.selected();
        t.delete(t.q0, t.q1);
        assert_eq!((t.all().as_str(), t.qh), ("; \n; ", 5));
        let chars: Vec<char> = s.chars().collect();
        let q = t.s.len();
        t.insert(q, &chars);
        assert_eq!(t.all(), "; \n; hello");
        assert!(t.read(100).is_none());
        t.send("echo hi");
        assert_eq!(t.read(100).unwrap(), b"helloecho hi\n");
        // Pasted typing with a newline may be read.
        t.paste("ls\nx");
        assert_eq!(t.selected(), "ls\nx");
        assert_eq!(t.read(100).unwrap(), b"ls\n");
    }

    #[test]
    fn double_clicks() {
        let t = text("; echo hello_there\nnext\n");
        assert_eq!(t.double(10), (7, 18));
        // At a line's start or end: the line.
        assert_eq!(t.double(0), (0, 19));
        assert_eq!(t.double(18), (0, 19));
        assert_eq!(t.double(19), (19, 24));
    }
}
