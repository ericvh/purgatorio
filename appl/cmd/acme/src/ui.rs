//! The row, its columns and their windows: where each tag and body is on
//! the screen, and how they are drawn.
//!
//! The screen is a row: a tag along the top and columns side by side
//! under it. A column is a tag and windows stacked in it; a window is a
//! tag line, with a small box at its left that is filled while the file
//! is modified, and a body with a scroll bar.

use std::collections::HashMap;

use purgatorio::Error;
use purgatorio::draw::{Display, Font, Image, Point, Rect};

use crate::frame::{self, Line, Widths};
use crate::text::Buffer;

/// The scroll bar's width and the gap after it (acme's `Scrollwid` and
/// `Scrollgap`).
pub const SCROLLWID: i32 = 12;
pub const SCROLLGAP: i32 = 4;
/// The border between columns and between windows.
pub const BORDER: i32 = 2;
/// The most lines a window's tag grows to.
pub const MAX_TAG_LINES: usize = 4;

/// What kind of text a [`Text`] is: it decides the colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tag,
    Body,
}

/// A sweep in progress with button 2 or 3, drawn in its own colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sweep {
    pub q0: usize,
    pub q1: usize,
    pub button: u32,
}

/// A tag or a body: its runes, its selection, the first rune shown and
/// where it is.
pub struct Text {
    pub b: Buffer,
    pub kind: Kind,
    pub q0: usize,
    pub q1: usize,
    /// The first rune shown.
    pub org: usize,
    /// Where the runes are drawn.
    pub r: Rect,
    /// The lines last laid out from `org`.
    pub lines: Vec<Line>,
    pub sweep: Option<Sweep>,
    /// Needs drawing.
    pub dirty: bool,
    /// Which of acme's fonts it is drawn in: 0 is the main one.
    pub font: usize,
    /// Its tab stops, in widths of `0`, when not acme's default.
    pub tab: Option<usize>,
}

impl Text {
    pub fn new(kind: Kind) -> Text {
        Text {
            b: Buffer::new(kind == Kind::Body),
            kind,
            q0: 0,
            q1: 0,
            org: 0,
            r: Rect::default(),
            lines: Vec::new(),
            sweep: None,
            dirty: true,
            font: 0,
            tab: None,
        }
    }

    pub fn with(kind: Kind, s: &str) -> Text {
        let mut t = Text::new(kind);
        t.b.insert(0, s);
        t.b.set_clean();
        let n = t.b.len();
        t.set_select(n, n);
        t
    }

    pub fn set_select(&mut self, q0: usize, q1: usize) {
        let n = self.b.len();
        let q0 = q0.min(n);
        let q1 = q1.min(n).max(q0);
        if (q0, q1) != (self.q0, self.q1) {
            self.q0 = q0;
            self.q1 = q1;
            self.dirty = true;
        }
    }

    pub fn selection(&self) -> String {
        self.b.string(self.q0, self.q1)
    }

    /// Replaces the selection with `s` and leaves the cursor after it.
    pub fn replace(&mut self, s: &str) {
        self.b.delete(self.q0, self.q1);
        self.b.insert(self.q0, s);
        let q = self.q0 + s.chars().count();
        self.q0 = q;
        self.q1 = q;
        self.fix_org();
        self.dirty = true;
    }

    /// Replaces runes `q0..q1` with `s`, as a write to acme(4)'s files
    /// does, keeping the selection and the first rune shown on the same
    /// text. A cursor at `q0` moves past the new text, so it follows
    /// output. Returns whether it did.
    pub fn edit(&mut self, q0: usize, q1: usize, s: &str) -> bool {
        let n = s.chars().count();
        let follow = self.q0 == self.q1 && self.q0 == q0 && n > 0;
        self.b.delete(q0, q1);
        self.b.insert(q0, s);
        let fix = |q: usize| {
            if q >= q1 {
                q - (q1 - q0) + n
            } else if q > q0 {
                q0
            } else {
                q
            }
        };
        if follow {
            self.q0 += n;
            self.q1 = self.q0;
        } else {
            self.q0 = fix(self.q0);
            self.q1 = fix(self.q1);
        }
        if self.org > q0 {
            self.org = fix(self.org);
        }
        self.fix_org();
        self.dirty = true;
        follow
    }

    /// Keeps `org` inside the text after a deletion.
    pub fn fix_org(&mut self) {
        if self.org > self.b.len() {
            self.org = self.b.len();
        }
    }

    /// How many lines fit.
    pub fn max_lines(&self, fh: i32) -> usize {
        (self.r.dy() / fh.max(1)).max(0) as usize
    }
}

/// A window: a tag and a body.
pub struct Window {
    pub id: u64,
    pub tag: Text,
    pub body: Text,
    /// The top of its tag.
    pub y: i32,
    /// Where it ends: the next window's top less the border, or the
    /// column's bottom.
    pub bottom: i32,
    /// The left and right of the column it is in.
    pub x0: i32,
    pub x1: i32,
    /// A `Del` or `Get` has warned that the file is modified.
    pub warned: bool,
    /// The body is a directory listing.
    pub is_dir: bool,
    /// How many lines the tag has: it grows to hold its text.
    pub tag_lines: i32,
    /// Typing a newline copies the line's leading blanks (`Indent`).
    pub indent: bool,
    /// Its state in acme(4)'s file system.
    pub fs: WinFs,
}

/// What a window's files on `/mnt/acme/<id>` keep.
#[derive(Default)]
pub struct WinFs {
    /// The address `addr` sets and `data` reads and writes.
    pub addr: (usize, usize),
    /// How many times `addr` is open: the first open sets it to `0,0`.
    pub naddr: usize,
    /// How many times `event` is open. While it is, button 2 and 3
    /// actions and changes to the text are events, not acted on.
    pub nevent: usize,
    /// Event text not read yet.
    pub events: Vec<u8>,
    /// `nomark`: writes to the files do not start new undo groups.
    pub nomark: bool,
    pub dump: Option<String>,
    pub dumpdir: Option<String>,
}

impl Window {
    pub fn new(id: u64, body: Text) -> Window {
        Window {
            id,
            tag: Text::with(Kind::Tag, ""),
            body,
            y: 0,
            bottom: 0,
            x0: 0,
            x1: 0,
            warned: false,
            is_dir: false,
            tag_lines: 1,
            indent: false,
            fs: WinFs::default(),
        }
    }

    /// The file name: the tag's first word.
    pub fn name(&self) -> String {
        let s = self.tag.b.all();
        s.split_whitespace().next().unwrap_or("").to_string()
    }

    /// Whether `Del` and `Exit` should warn before throwing the body away.
    /// Unnamed windows, directories and `+Errors` windows never are.
    pub fn modified(&self) -> bool {
        let n = self.name();
        self.body.b.dirty() && !n.is_empty() && !self.is_dir && !is_scratch(&n)
    }

    /// The directory commands from this window run in, and relative names
    /// in it are looked up from.
    pub fn dir(&self, cwd: &str) -> String {
        let n = self.name();
        if n.is_empty() {
            return cwd.to_string();
        }
        if self.is_dir || n.ends_with('/') {
            return n.trim_end_matches('/').to_string();
        }
        match n.rfind('/') {
            Some(0) => String::from("/"),
            Some(i) => n[..i].to_string(),
            None => cwd.to_string(),
        }
    }

    /// The box at the tag's left.
    pub fn boxr(&self, fh: i32) -> Rect {
        Rect::new(self.x0, self.y, self.x0 + SCROLLWID, self.y + fh)
    }

    /// The body's scroll bar.
    pub fn scrollr(&self) -> Rect {
        Rect::new(
            self.x0,
            self.body.r.min.y,
            self.x0 + SCROLLWID,
            self.body.r.max.y,
        )
    }

    /// How many lines the tag needs for its text, at most
    /// [`MAX_TAG_LINES`] and no more than the window has.
    pub fn tag_lines_needed(&self, fh: i32, f: &mut Fnt) -> i32 {
        let tx = self.x0 + SCROLLWID + SCROLLGAP;
        f.use_tab(None);
        let n = frame::layout(&self.tag.b, 0, self.x1 - tx, MAX_TAG_LINES, f).len() as i32;
        n.clamp(1, ((self.bottom - self.y - 1) / fh).max(1))
    }

    /// Places the tag and body from `y`, `bottom`, `x0` and `x1`.
    pub fn place(&mut self, fh: i32, f: &mut Fnt) {
        let tx = self.x0 + SCROLLWID + SCROLLGAP;
        self.tag_lines = self.tag_lines_needed(fh, f);
        self.tag.org = 0;
        let th = self.tag_lines * fh;
        self.tag.r = Rect::new(tx, self.y, self.x1, self.y + th);
        let by = (self.y + th + 1).min(self.bottom);
        self.body.r = Rect::new(tx, by, self.x1, self.bottom);
        self.tag.dirty = true;
        self.body.dirty = true;
    }
}

/// `+Errors` windows hold output and are never asked to be saved.
pub fn is_scratch(name: &str) -> bool {
    name == "+Errors" || name.ends_with("/+Errors")
}

pub struct Column {
    pub tag: Text,
    pub wins: Vec<Window>,
    pub r: Rect,
}

impl Column {
    /// The top of the area windows are in.
    pub fn top(&self, fh: i32) -> i32 {
        self.r.min.y + fh + 1
    }

    /// Lays out the column's tag and windows in `r`, keeping each
    /// window's top and making every window at least a tag high.
    pub fn place(&mut self, fh: i32, f: &mut Fnt) {
        let tx = self.r.min.x + SCROLLWID + SCROLLGAP;
        self.tag.r = Rect::new(tx, self.r.min.y, self.r.max.x, self.r.min.y + fh);
        self.tag.dirty = true;
        let top = self.top(fh);
        let n = self.wins.len();
        let min = fh + 1;
        for i in 0..n {
            let lo = if i == 0 {
                top
            } else {
                self.wins[i - 1].y + min + BORDER
            };
            // Leave room for the windows after this one.
            let hi = self.r.max.y - (n - i) as i32 * (min + BORDER) + BORDER;
            let y = if i == 0 { top } else { self.wins[i].y };
            self.wins[i].y = y.max(lo).min(hi.max(lo));
        }
        for i in 0..n {
            let bottom = if i + 1 < n {
                self.wins[i + 1].y - BORDER
            } else {
                self.r.max.y
            };
            let w = &mut self.wins[i];
            w.bottom = bottom;
            w.x0 = self.r.min.x;
            w.x1 = self.r.max.x;
            w.place(fh, f);
        }
    }

    /// Where a new window goes: the lower half of the tallest window, or
    /// all of an empty column. Returns the index it is inserted at and
    /// its top.
    pub fn slot(&self, fh: i32) -> (usize, i32) {
        let Some((i, w)) = self
            .wins
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| w.bottom - w.y)
        else {
            return (0, self.top(fh));
        };
        let h = w.bottom - w.y;
        let y = if h >= 2 * (fh + 1) + BORDER {
            w.y + h / 2
        } else {
            w.bottom
        };
        (i + 1, y)
    }
}

/// The colours, as acme has them.
pub struct Colours {
    pub tag_bg: Image,
    pub tag_high: Image,
    pub body_bg: Image,
    pub body_high: Image,
    pub border: Image,
    pub button: Image,
    pub modified: Image,
    pub scroll_bg: Image,
    pub text: Image,
    pub white: Image,
    pub red: Image,
    pub green: Image,
}

impl Colours {
    pub fn new(d: &mut Display) -> Result<Colours, Error> {
        Ok(Colours {
            tag_bg: d.color(0xEAFF_FFFF)?,
            tag_high: d.color(0x9EEE_EEFF)?,
            body_bg: d.color(0xFFFF_EAFF)?,
            body_high: d.color(0xEEEE_9EFF)?,
            border: d.color(0x8888_CCFF)?,
            button: d.color(0x8888_CCFF)?,
            modified: d.color(0x0000_99FF)?,
            scroll_bg: d.color(0x9999_4CFF)?,
            text: d.color(0x0000_00FF)?,
            white: d.color(0xFFFF_FFFF)?,
            red: d.color(0xAA00_00FF)?,
            green: d.color(0x0066_00FF)?,
        })
    }
}

/// A font with the widths of its runes remembered.
pub struct Fnt {
    pub font: Font,
    /// The file it came from.
    pub name: String,
    widths: HashMap<char, i32>,
    /// The width of `0`, which tab stops are counted in.
    zero: i32,
    tab: i32,
}

/// acme's default tab stop: four zeros.
pub const TAB: usize = 4;

impl Fnt {
    pub fn new(mut font: Font, name: &str) -> Fnt {
        let zero = font.string_width("0").max(1);
        Fnt {
            font,
            name: name.to_string(),
            widths: HashMap::new(),
            zero,
            tab: TAB as i32 * zero,
        }
    }

    /// Sets the tab stops for the text laid out next: every `n` zeros,
    /// or the default.
    pub fn use_tab(&mut self, n: Option<usize>) {
        self.tab = n.unwrap_or(TAB).clamp(1, 100) as i32 * self.zero;
    }

    pub fn height(&self) -> i32 {
        self.font.height.max(1)
    }
}

impl Widths for Fnt {
    fn width(&mut self, c: char) -> i32 {
        if let Some(w) = self.widths.get(&c) {
            return *w;
        }
        let mut b = [0u8; 4];
        let w = self.font.string_width(c.encode_utf8(&mut b));
        self.widths.insert(c, w);
        w
    }

    fn tab(&mut self) -> i32 {
        self.tab
    }
}

/// Draws text `t` on `dst`: background, selection or sweep, runes and
/// the cursor tick. Lays it out again first.
pub fn draw_text(
    d: &mut Display,
    dst: &Image,
    t: &mut Text,
    c: &Colours,
    f: &mut Fnt,
) -> Result<(), Error> {
    f.use_tab(t.tab);
    let fh = f.height();
    let (bg, high) = match t.kind {
        Kind::Tag => (&c.tag_bg, &c.tag_high),
        Kind::Body => (&c.body_bg, &c.body_high),
    };
    let zero = Point::new(0, 0);
    if t.r.is_empty() {
        t.lines.clear();
        t.dirty = false;
        return Ok(());
    }
    d.draw(dst, t.r, bg, None, zero)?;
    t.lines = frame::layout(&t.b, t.org, t.r.dx(), t.max_lines(fh), f);
    let (hq0, hq1, hcol, tcol) = match t.sweep {
        Some(s) => (
            s.q0,
            s.q1,
            if s.button == 2 { &c.red } else { &c.green },
            &c.white,
        ),
        None => (t.q0, t.q1, high, &c.text),
    };
    for (i, l) in t.lines.iter().enumerate() {
        let y = t.r.min.y + i as i32 * fh;
        // The highlight: within the line, and to its right edge when it
        // goes on past the line's end.
        let s0 = hq0.max(l.q0);
        let s1 = hq1.min(l.next);
        if hq0 < hq1 && s0 < s1 {
            let x0 = t.r.min.x + frame::x_of(&t.b, l, s0, f);
            let x1 = if hq1 > l.q1 && l.next > l.q1 {
                t.r.max.x
            } else {
                t.r.min.x + frame::x_of(&t.b, l, s1, f)
            };
            d.draw(dst, Rect::new(x0, y, x1, y + fh), hcol, None, zero)?;
        }
        run(d, dst, t, l, l.q0, l.q1, y, &c.text, f)?;
        if t.sweep.is_some() && s0 < s1.min(l.q1) {
            run(d, dst, t, l, s0, s1.min(l.q1), y, tcol, f)?;
        }
    }
    if t.q0 == t.q1
        && t.sweep.is_none()
        && let Some(i) = frame::line_of(&t.lines, t.q0)
    {
        let l = t.lines[i];
        let x = t.r.min.x + frame::x_of(&t.b, &l, t.q0, f);
        let y = t.r.min.y + i as i32 * fh;
        let x = x.min(t.r.max.x - 2).max(t.r.min.x + 1);
        // A tick with small boxes at its ends.
        d.draw(dst, Rect::new(x, y, x + 1, y + fh), &c.text, None, zero)?;
        d.draw(dst, Rect::new(x - 1, y, x + 2, y + 3), &c.text, None, zero)?;
        d.draw(
            dst,
            Rect::new(x - 1, y + fh - 3, x + 2, y + fh),
            &c.text,
            None,
            zero,
        )?;
    }
    t.dirty = false;
    Ok(())
}

/// Draws runes `q0..q1` of line `l` (at height `y`) in `col`, a string
/// per stretch between tabs.
#[allow(clippy::too_many_arguments)]
fn run(
    d: &mut Display,
    dst: &Image,
    t: &Text,
    l: &Line,
    q0: usize,
    q1: usize,
    y: i32,
    col: &Image,
    f: &mut Fnt,
) -> Result<(), Error> {
    let mut x = frame::x_of(&t.b, l, q0, f);
    let mut s = String::new();
    let mut sx = x;
    let zero = Point::new(0, 0);
    for q in q0..q1 {
        let ch = t.b.at(q);
        let a = frame::advance(f, ch, x);
        if ch == '\t' {
            if !s.is_empty() {
                d.string(
                    dst,
                    Point::new(t.r.min.x + sx, y),
                    col,
                    zero,
                    &mut f.font,
                    &s,
                )?;
                s.clear();
            }
            sx = x + a;
        } else {
            s.push(ch);
        }
        x += a;
    }
    if !s.is_empty() {
        d.string(
            dst,
            Point::new(t.r.min.x + sx, y),
            col,
            zero,
            &mut f.font,
            &s,
        )?;
    }
    Ok(())
}

/// Draws a body's scroll bar: the part of the text shown is the light
/// thumb.
pub fn draw_scroll(d: &mut Display, dst: &Image, w: &Window, c: &Colours) -> Result<(), Error> {
    let r = w.scrollr();
    if r.is_empty() {
        return Ok(());
    }
    let zero = Point::new(0, 0);
    d.draw(dst, r, &c.scroll_bg, None, zero)?;
    let len = w.body.b.len();
    let end = w.body.lines.last().map_or(len, |l| l.next.max(l.q1));
    let h = r.dy() as i64;
    let (t0, t1) = if w.body.b.is_empty() {
        (0, h)
    } else {
        (
            h * w.body.org as i64 / len as i64,
            h * end as i64 / len as i64,
        )
    };
    let mut y0 = r.min.y + t0 as i32;
    let mut y1 = r.min.y + t1 as i32;
    if y1 - y0 < 2 {
        y1 = (y0 + 2).min(r.max.y);
        y0 = y1 - 2;
    }
    d.draw(
        dst,
        Rect::new(r.min.x, y0, r.max.x - 1, y1),
        &c.body_bg,
        None,
        zero,
    )?;
    Ok(())
}

/// Draws a window's box: a square, filled dark while it is modified.
pub fn draw_box(
    d: &mut Display,
    dst: &Image,
    w: &Window,
    c: &Colours,
    fh: i32,
) -> Result<(), Error> {
    let r = w.boxr(fh);
    let zero = Point::new(0, 0);
    d.draw(dst, r, &c.tag_bg, None, zero)?;
    let inner = Rect::new(r.min.x + 1, r.min.y + 1, r.max.x - 1, r.max.y - 1);
    let col = if w.modified() { &c.modified } else { &c.button };
    d.draw(dst, inner, col, None, zero)?;
    Ok(())
}
