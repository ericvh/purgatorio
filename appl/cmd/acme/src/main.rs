//! acme: a text editor and window system on `/dev/draw`, after Inferno's
//! acme(1).
//!
//! `acme [-f font] [file ...]` fills the screen with a row of columns of
//! windows, each a tag line and a body, and opens the files given in the
//! first column. The mouse does the work:
//!
//! - Button 1 selects text (a double click selects a word or a line);
//!   with button 1 held, button 2 cuts and button 3 pastes.
//! - Button 2 executes the word under it, or the text it sweeps, or the
//!   selection when clicked in it: a built-in command (`Cut`, `Del`,
//!   `Delcol`, `Exit`, `Get`, `Look`, `New`, `Newcol`, `Paste`, `Put`,
//!   `Putall`, `Redo`, `Snarf`, `Undo`), or else a command line for `sh`
//!   run in the window's directory, whose output goes to the directory's
//!   `+Errors` window.
//! - Button 3 opens the file or directory named under it (with an
//!   optional `:line`), or else searches the body for the text.
//! - In a scroll bar, button 1 scrolls back, button 3 forward and
//!   button 2 to a place; the wheel scrolls too.
//!
//! Typed text goes to the tag or body under the pointer and replaces its
//! selection. Backspace, `^U`, `^W`, `^A`, `^E`, Escape and the arrows,
//! Page Up, Page Down, Home and End work as in acme.
//!
//! acme reads `/dev/pointer` and `/dev/keyboard`, the pipes from the
//! commands it runs and the pipe its file system (acme(4), on
//! `/mnt/acme`, see [`fs`]) is served on, by waiting for whichever is
//! ready ([`purgatorio::ready`]), so one prog does it all.

mod addr;
mod frame;
mod fs;
mod text;
mod ui;

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;

use purgatorio::abi::pctl;
use purgatorio::draw::{Display, Font, Image, Keyboard, Mouse, Point, Pointer};
use purgatorio::{Context, Error, Ready};

use text::{is_file, is_word};
use ui::{BORDER, Colours, Column, Fnt, Kind, SCROLLWID, Sweep, Text, Window};

/// The tags of the row and of a new column.
const ROW_TAG: &str = "Newcol Putall Exit ";
const COL_TAG: &str = "New Delcol ";

/// The commands acme runs itself; others go to `sh`.
const BUILTINS: &[&str] = &[
    "Cut", "Del", "Delcol", "Delete", "Exit", "Get", "Look", "New", "Newcol", "Paste", "Put",
    "Putall", "Redo", "Snarf", "Undo",
];

/// Whether `path` is one of acme's own files, which acme must not open
/// itself: it would wait for its own answer.
fn own(path: &str) -> bool {
    path == fs::MTPT || path.starts_with("/mnt/acme/")
}

/// keyboard(6)'s codes for the keys acme uses.
const KHOME: char = '\u{F00D}';
const KUP: char = '\u{F00E}';
const KPGUP: char = '\u{F00F}';
const KLEFT: char = '\u{F011}';
const KRIGHT: char = '\u{F012}';
const KPGDOWN: char = '\u{F013}';
const KEND: char = '\u{F018}';
const KDOWN: char = '\u{F800}';

/// Why acme will not open a file under `/mnt/acme`.
const EOWN: &str = "acme's own file";

/// How close in time two clicks are to make a double click.
const DOUBLE_MS: u32 = 500;

/// Which text: the row's tag, a column's, or a window's tag or body
/// (windows by id, which stay the same while windows come and go).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tid {
    Row,
    Col(usize),
    Tag(u64),
    Body(u64),
}

/// What is under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Text(Tid),
    Scroll(u64),
    Box(u64),
    Nothing,
}

/// A command acme started: the pipe its output comes on.
struct Child {
    fd: File,
    dir: String,
    /// Bytes of a rune split between reads.
    carry: Vec<u8>,
}

struct Acme {
    d: Display,
    screen: Image,
    c: Colours,
    f: Fnt,
    tag: Text,
    cols: Vec<Column>,
    mouse: Mouse,
    kbd: Keyboard,
    /// The last pointer state.
    m: Pointer,
    next_id: u64,
    snarf: String,
    /// The text last selected or typed in, which `Snarf`, `Cut` and
    /// `Paste` in a row or column tag act on.
    seltext: Option<Tid>,
    /// Where typing started, for Escape.
    typing: Option<(Tid, usize)>,
    /// The last button 1 click, for double clicks.
    click: Option<(Tid, usize, u32)>,
    children: Vec<Child>,
    /// `/prog/<pid>/wait`, read so the exit messages of commands do not
    /// pile up.
    waitfd: Option<File>,
    cwd: String,
    /// The font's file, for acme(4)'s `ctl`.
    font_name: String,
    /// The file system on `/mnt/acme`, when it could be mounted.
    fs: Option<fs::Fs>,
    /// Redraw everything.
    full: bool,
    exit_warned: bool,
    exit: bool,
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Cleans a path: no `.`, no `x/..`, no doubled slashes.
fn clean(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !abs {
                    out.push("..");
                }
            }
            _ => out.push(p),
        }
    }
    let s = out.join("/");
    if abs {
        format!("/{s}")
    } else if s.is_empty() {
        String::from(".")
    } else {
        s
    }
}

/// Splits `name:addr` into the name and a line number.
fn split_addr(s: &str) -> (&str, Option<usize>) {
    if let Some((n, a)) = s.rsplit_once(':')
        && let Ok(line) = a.parse::<usize>()
    {
        return (n, Some(line));
    }
    (s.trim_end_matches(':'), None)
}

impl Acme {
    fn new(font: Font, font_name: String) -> Result<Acme, Error> {
        let mut d = Display::open()?;
        let screen = d.screen();
        let c = Colours::new(&mut d)?;
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| String::from("/"));
        let waitfd = File::open(format!("/prog/{}/wait", purgatorio::pid())).ok();
        let mut a = Acme {
            d,
            screen,
            c,
            f: Fnt::new(font),
            tag: Text::with(Kind::Tag, ROW_TAG),
            cols: Vec::new(),
            mouse: Mouse::open()?,
            kbd: Keyboard::open()?,
            m: Pointer::default(),
            next_id: 1,
            snarf: String::new(),
            seltext: None,
            typing: None,
            click: None,
            children: Vec::new(),
            waitfd,
            cwd,
            font_name,
            fs: None,
            full: true,
            exit_warned: false,
            exit: false,
        };
        a.new_col();
        a.new_col();
        Ok(a)
    }

    fn fh(&self) -> i32 {
        self.f.height()
    }

    // ---- Layout.

    /// Places everything after the screen's size or the columns change.
    fn place(&mut self) {
        let fh = self.fh();
        let r = self.screen.r;
        let tx = r.min.x + SCROLLWID + ui::SCROLLGAP;
        self.tag.r = Rect::new(tx, r.min.y, r.max.x, r.min.y + fh);
        self.tag.dirty = true;
        let y0 = r.min.y + fh + 1;
        let n = self.cols.len();
        for i in 0..n {
            let x1 = if i + 1 < n {
                self.cols[i + 1].r.min.x - BORDER
            } else {
                r.max.x
            };
            let c = &mut self.cols[i];
            c.r = Rect::new(c.r.min.x, y0, x1, r.max.y);
            c.place(fh, &mut self.f);
        }
        self.full = true;
    }

    /// Fits the columns and windows to a new screen rectangle, keeping
    /// their proportions.
    fn resized(&mut self, old: Rect) {
        let new = self.screen.r;
        let (ow, nw) = (old.dx().max(1), new.dx());
        let fh = self.fh();
        let (oy, ny) = (old.min.y + fh + 1, new.min.y + fh + 1);
        let (oh, nh) = ((old.max.y - oy).max(1), new.max.y - ny);
        for c in &mut self.cols {
            c.r.min.x = new.min.x + (c.r.min.x - old.min.x) * nw / ow;
            for w in &mut c.wins {
                w.y = ny + (w.y - oy) * nh / oh;
            }
        }
        if let Some(c) = self.cols.first_mut() {
            c.r.min.x = new.min.x;
        }
        self.place();
    }

    fn new_col(&mut self) {
        let r = self.screen.r;
        let x = match self.cols.last() {
            None => r.min.x,
            Some(c) => (c.r.min.x + c.r.max.x) / 2,
        };
        self.cols.push(Column {
            tag: Text::with(Kind::Tag, COL_TAG),
            wins: Vec::new(),
            r: Rect::new(x, r.min.y, r.max.x, r.max.y),
        });
        self.place();
    }

    fn del_col(&mut self, ci: usize) {
        if let Some(w) = self.cols[ci]
            .wins
            .iter()
            .find(|w| w.modified() && !w.warned)
        {
            let name = w.name();
            let id = w.id;
            self.error("", &format!("{name} modified"));
            if let Some(w) = self.win_mut(id) {
                w.warned = true;
            }
            return;
        }
        let x0 = self.cols[ci].r.min.x;
        self.cols.remove(ci);
        if ci == 0
            && let Some(c) = self.cols.first_mut()
        {
            c.r.min.x = x0;
        }
        self.seltext = None;
        self.place();
    }

    /// Makes a window in column `ci` with tag name `name` and body `body`.
    fn new_win(&mut self, ci: usize, name: &str, body: &str, is_dir: bool) -> u64 {
        if self.cols.is_empty() {
            self.new_col();
        }
        let ci = ci.min(self.cols.len() - 1);
        let fh = self.fh();
        let id = self.next_id;
        self.next_id += 1;
        let mut w = Window {
            id,
            tag: Text::with(Kind::Tag, ""),
            body: Text::new(Kind::Body),
            y: 0,
            bottom: 0,
            x0: 0,
            x1: 0,
            warned: false,
            is_dir,
            tag_lines: 1,
            fs: ui::WinFs::default(),
        };
        w.body.b.load(body);
        set_tag(&mut w, name);
        // The cursor goes at the end of the tag, ready to type a command.
        let n = w.tag.b.len();
        w.tag.set_select(n, n);
        let (i, y) = self.cols[ci].slot(fh);
        w.y = y;
        self.cols[ci].wins.insert(i, w);
        self.cols[ci].place(fh, &mut self.f);
        self.full = true;
        id
    }

    fn del_win(&mut self, id: u64) {
        let Some((ci, wi)) = self.find(id) else {
            return;
        };
        let c = &mut self.cols[ci];
        let top = c.wins[wi].y;
        c.wins.remove(wi);
        if wi < c.wins.len() && wi == 0 {
            c.wins[0].y = top;
        }
        let fh = self.fh();
        self.cols[ci].place(fh, &mut self.f);
        if matches!(self.seltext, Some(Tid::Tag(i) | Tid::Body(i)) if i == id) {
            self.seltext = None;
        }
        self.full = true;
    }

    // ---- Finding things.

    fn find(&self, id: u64) -> Option<(usize, usize)> {
        for (ci, c) in self.cols.iter().enumerate() {
            if let Some(wi) = c.wins.iter().position(|w| w.id == id) {
                return Some((ci, wi));
            }
        }
        None
    }

    /// The ids of the windows, in order.
    fn ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .cols
            .iter()
            .flat_map(|c| c.wins.iter().map(|w| w.id))
            .collect();
        v.sort_unstable();
        v
    }

    fn win(&self, id: u64) -> Option<&Window> {
        let (ci, wi) = self.find(id)?;
        Some(&self.cols[ci].wins[wi])
    }

    fn win_mut(&mut self, id: u64) -> Option<&mut Window> {
        let (ci, wi) = self.find(id)?;
        Some(&mut self.cols[ci].wins[wi])
    }

    fn by_name(&self, name: &str) -> Option<u64> {
        self.cols
            .iter()
            .flat_map(|c| c.wins.iter())
            .find(|w| w.name() == name)
            .map(|w| w.id)
    }

    fn text(&self, t: Tid) -> Option<&Text> {
        match t {
            Tid::Row => Some(&self.tag),
            Tid::Col(i) => self.cols.get(i).map(|c| &c.tag),
            Tid::Tag(id) => self.win(id).map(|w| &w.tag),
            Tid::Body(id) => self.win(id).map(|w| &w.body),
        }
    }

    fn text_mut(&mut self, t: Tid) -> Option<&mut Text> {
        match t {
            Tid::Row => Some(&mut self.tag),
            Tid::Col(i) => self.cols.get_mut(i).map(|c| &mut c.tag),
            Tid::Tag(id) => self.win_mut(id).map(|w| &mut w.tag),
            Tid::Body(id) => self.win_mut(id).map(|w| &mut w.body),
        }
    }

    /// A text and the font, borrowed together.
    fn text_font(&mut self, t: Tid) -> Option<(&mut Text, &mut Fnt)> {
        let at = match t {
            Tid::Tag(id) | Tid::Body(id) => Some(self.find(id)?),
            _ => None,
        };
        let x = match (t, at) {
            (Tid::Row, _) => &mut self.tag,
            (Tid::Col(i), _) => &mut self.cols.get_mut(i)?.tag,
            (Tid::Tag(_), Some((ci, wi))) => &mut self.cols[ci].wins[wi].tag,
            (Tid::Body(_), Some((ci, wi))) => &mut self.cols[ci].wins[wi].body,
            _ => return None,
        };
        Some((x, &mut self.f))
    }

    /// The window a text belongs to.
    fn win_of(t: Tid) -> Option<u64> {
        match t {
            Tid::Tag(id) | Tid::Body(id) => Some(id),
            _ => None,
        }
    }

    /// The column a text is in: for the row's tag, the last column.
    fn col_of(&self, t: Tid) -> usize {
        match t {
            Tid::Row => self.cols.len().saturating_sub(1),
            Tid::Col(i) => i,
            Tid::Tag(id) | Tid::Body(id) => self.find(id).map_or(0, |(ci, _)| ci),
        }
    }

    fn dir_of(&self, t: Tid) -> String {
        Acme::win_of(t)
            .and_then(|id| self.win(id))
            .map_or_else(|| self.cwd.clone(), |w| w.dir(&self.cwd))
    }

    fn hit(&self, p: Point) -> Hit {
        let fh = self.fh();
        if p.y < self.tag.r.max.y {
            return Hit::Text(Tid::Row);
        }
        for (ci, c) in self.cols.iter().enumerate() {
            if p.x < c.r.min.x || p.x >= c.r.max.x {
                continue;
            }
            if p.y < c.top(fh) {
                return Hit::Text(Tid::Col(ci));
            }
            for w in &c.wins {
                if p.y < w.y || p.y >= w.bottom {
                    continue;
                }
                if p.y < w.tag.r.max.y {
                    if p.x < w.tag.r.min.x {
                        return Hit::Box(w.id);
                    }
                    return Hit::Text(Tid::Tag(w.id));
                }
                if p.x < w.body.r.min.x - ui::SCROLLGAP {
                    return Hit::Scroll(w.id);
                }
                return Hit::Text(Tid::Body(w.id));
            }
        }
        Hit::Nothing
    }

    /// The rune nearest `p` in text `t`.
    fn pos(&mut self, t: Tid, p: Point) -> usize {
        let fh = self.fh();
        let Some(tx) = self.text(t) else {
            return 0;
        };
        let lines = tx.lines.clone();
        let r = tx.r;
        let Some(last) = lines.len().checked_sub(1) else {
            return tx.org;
        };
        let i = ((p.y - r.min.y).max(0) / fh) as usize;
        if i > last {
            return lines[last].q1;
        }
        let Some((tx, f)) = self.text_font(t) else {
            return 0;
        };
        frame::q_at(&tx.b, &lines[i], p.x - r.min.x, f)
    }

    // ---- Drawing.

    fn redraw(&mut self) -> Result<(), Error> {
        let fh = self.fh();
        let mut any = false;
        let zero = Point::new(0, 0);
        // A tag whose text no longer fits its lines, or fits in fewer,
        // changes the window's layout.
        for c in &mut self.cols {
            if c.wins
                .iter()
                .any(|w| w.tag_lines != w.tag_lines_needed(fh, &mut self.f))
            {
                c.place(fh, &mut self.f);
                self.full = true;
            }
        }
        if self.full {
            let r = self.screen.r;
            self.d.draw(&self.screen, r, &self.c.border, None, zero)?;
            // The space left of each tag, beside its box.
            self.d.draw(
                &self.screen,
                Rect::new(r.min.x, r.min.y, self.tag.r.min.x, self.tag.r.max.y),
                &self.c.tag_bg,
                None,
                zero,
            )?;
            for c in &self.cols {
                self.d.draw(
                    &self.screen,
                    Rect::new(c.r.min.x, c.r.min.y, c.tag.r.min.x, c.tag.r.max.y),
                    &self.c.tag_bg,
                    None,
                    zero,
                )?;
                // A column without windows is body-coloured below its tag.
                let top = c.top(fh);
                let first = c.wins.first().map_or(c.r.max.y, |w| w.y);
                if first > top {
                    self.d.draw(
                        &self.screen,
                        Rect::new(c.r.min.x, top, c.r.max.x, first),
                        &self.c.body_bg,
                        None,
                        zero,
                    )?;
                }
            }
            self.tag.dirty = true;
            for c in &mut self.cols {
                c.tag.dirty = true;
                for w in &mut c.wins {
                    w.tag.dirty = true;
                    w.body.dirty = true;
                }
            }
            self.full = false;
            any = true;
        }
        let (d, s, col, f) = (&mut self.d, &self.screen, &self.c, &mut self.f);
        if self.tag.dirty {
            ui::draw_text(d, s, &mut self.tag, col, f)?;
            any = true;
        }
        for c in &mut self.cols {
            if c.tag.dirty {
                ui::draw_text(d, s, &mut c.tag, col, f)?;
                any = true;
            }
            for w in &mut c.wins {
                if w.tag.dirty {
                    ui::draw_text(d, s, &mut w.tag, col, f)?;
                    ui::draw_box(d, s, w, col, fh)?;
                    // The line between tag and body.
                    let y = w.tag.r.max.y;
                    if y < w.bottom {
                        d.draw(s, Rect::new(w.x0, y, w.x1, y + 1), &col.border, None, zero)?;
                    }
                    any = true;
                }
                if w.body.dirty {
                    ui::draw_text(d, s, &mut w.body, col, f)?;
                    ui::draw_scroll(d, s, w, col)?;
                    // The gap between the scroll bar and the text.
                    let r = w.body.r;
                    d.draw(
                        s,
                        Rect::new(w.x0 + SCROLLWID, r.min.y, r.min.x, r.max.y),
                        &col.body_bg,
                        None,
                        zero,
                    )?;
                    any = true;
                }
            }
        }
        if any {
            self.d.flush()?;
        }
        Ok(())
    }

    fn touch(&mut self, t: Tid) {
        if let Some(x) = self.text_mut(t) {
            x.dirty = true;
        }
    }

    // ---- Tags.

    /// Rewrites a window's tag after its body changes.
    fn retag(&mut self, id: u64) {
        if let Some(w) = self.win_mut(id) {
            let name = w.name();
            set_tag(w, &name);
            w.tag.dirty = true;
        }
    }

    // ---- Scrolling.

    /// Scrolls body `id` by `n` lines, forward if `n` is positive.
    fn scroll(&mut self, id: u64, n: i32) {
        let Some((ci, wi)) = self.find(id) else {
            return;
        };
        let w = &mut self.cols[ci].wins[wi];
        let t = &mut w.body;
        let width = t.r.dx();
        if n < 0 {
            t.org = frame::back(&t.b, t.org, (-n) as usize, width, &mut self.f);
        } else if n > 0 {
            let lines = frame::layout(&t.b, t.org, width, n as usize + 1, &mut self.f);
            t.org = match lines.get(n as usize) {
                Some(l) => l.q0,
                None => lines.last().map_or(t.org, |l| {
                    if l.next > l.q1 {
                        l.next
                    } else {
                        frame::wrapped_start(&t.b, l.q0, width, &mut self.f)
                    }
                }),
            };
        }
        t.dirty = true;
    }

    /// Scrolls so that `q` is shown, a third of the way down when it was
    /// not.
    fn show(&mut self, t: Tid, q: usize) {
        let fh = self.fh();
        let Some((tx, f)) = self.text_font(t) else {
            return;
        };
        let (org, width, max) = (tx.org, tx.r.dx(), tx.max_lines(fh));
        if max == 0 {
            return;
        }
        let lines = frame::layout(&tx.b, org, width, max, f);
        if q >= org && frame::line_of(&lines, q).is_some() {
            return;
        }
        let s = frame::wrapped_start(&tx.b, q, width, f);
        tx.org = frame::back(&tx.b, s, max / 3, width, f);
        tx.dirty = true;
    }

    // ---- Mouse.

    fn mouse(&mut self, m: Pointer) -> Result<(), Error> {
        let old = self.screen.r;
        if self.d.check_resize()? {
            self.screen = self.d.screen();
            self.resized(old);
        }
        let pressed = m.buttons & !self.m.buttons;
        self.m = m;
        if pressed == 0 {
            return Ok(());
        }
        let h = self.hit(m.xy);
        if pressed & (8 | 16) != 0 {
            let id = match h {
                Hit::Text(Tid::Body(id) | Tid::Tag(id)) | Hit::Scroll(id) | Hit::Box(id) => id,
                _ => return Ok(()),
            };
            self.scroll(id, if pressed & 8 != 0 { -3 } else { 3 });
            return Ok(());
        }
        self.typing = None;
        match h {
            Hit::Scroll(id) => self.scrollbar(id, m)?,
            Hit::Text(t) => {
                if let Some(x) = self.text_mut(t) {
                    x.b.mark();
                }
                if pressed & 1 != 0 {
                    self.select(t, m)?;
                } else if pressed & 2 != 0 {
                    if let Some((q0, q1)) = self.sweep(t, m, 2)?
                        && !self.send_event(t, q0, q1, 'X')
                    {
                        let s = self.expand(t, q0, q1, false);
                        self.execute(t, &s)?;
                    }
                } else if pressed & 4 != 0
                    && let Some((q0, q1)) = self.sweep(t, m, 4)?
                    && !self.send_event(t, q0, q1, 'L')
                {
                    let s = self.expand(t, q0, q1, true);
                    self.look(t, &s)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Reads the pointer until every button is up, calling `f` on each
    /// state.
    fn track(
        &mut self,
        mut f: impl FnMut(&mut Acme, Pointer) -> Result<(), Error>,
    ) -> Result<(), Error> {
        while self.m.buttons & 7 != 0 {
            self.redraw()?;
            let m = self.mouse.read()?;
            f(self, m)?;
            self.m = m;
        }
        Ok(())
    }

    /// Button 1: selects, extending with the pointer; a double click
    /// selects a word or line; buttons 2 and 3 while it is held cut and
    /// paste.
    fn select(&mut self, t: Tid, m: Pointer) -> Result<(), Error> {
        let q = self.pos(t, m.xy);
        self.seltext = Some(t);
        let double = matches!(self.click, Some((ct, cq, ms))
            if ct == t && cq == q && m.msec.wrapping_sub(ms) < DOUBLE_MS);
        if double {
            self.click = None;
            if let Some(x) = self.text_mut(t) {
                let (q0, q1) = double_click(&x.b, q);
                x.set_select(q0, q1);
            }
        } else if let Some(x) = self.text_mut(t) {
            x.set_select(q, q);
        }
        let anchor = q;
        let mut moved = false;
        let mut chorded = false;
        self.track(|a, n| {
            let new = n.buttons & !a.m.buttons;
            if new & 2 != 0 {
                chorded = true;
                a.cut(t, true)?;
            }
            if new & 4 != 0 {
                chorded = true;
                a.paste(t)?;
            }
            if n.buttons & 1 != 0 && !chorded && !double {
                let p = a.pos(t, n.xy);
                if p != anchor {
                    moved = true;
                }
                if let Some(x) = a.text_mut(t) {
                    x.set_select(anchor.min(p), anchor.max(p));
                }
            }
            Ok(())
        })?;
        if !double && !moved && !chorded {
            self.click = Some((t, q, m.msec));
        }
        Ok(())
    }

    /// Buttons 2 and 3: sweeps text in red or green and returns it when
    /// the button comes up, unless another button was pressed to cancel.
    fn sweep(&mut self, t: Tid, m: Pointer, button: u32) -> Result<Option<(usize, usize)>, Error> {
        let q = self.pos(t, m.xy);
        let colour = if button == 2 { 2 } else { 3 };
        if let Some(x) = self.text_mut(t) {
            x.sweep = Some(Sweep {
                q0: q,
                q1: q,
                button: colour,
            });
            x.dirty = true;
        }
        let mut cancel = false;
        let mut end = q;
        self.track(|a, n| {
            if n.buttons & !button & 7 != 0 {
                cancel = true;
            }
            if n.buttons & button != 0 {
                end = a.pos(t, n.xy);
                if let Some(x) = a.text_mut(t) {
                    x.sweep = Some(Sweep {
                        q0: q.min(end),
                        q1: q.max(end),
                        button: colour,
                    });
                    x.dirty = true;
                }
            }
            Ok(())
        })?;
        if let Some(x) = self.text_mut(t) {
            x.sweep = None;
            x.dirty = true;
        }
        Ok((!cancel).then_some((q.min(end), q.max(end))))
    }

    /// The text a button 2 or 3 click means: what was swept, else the
    /// selection when the click was in it, else the word (for button 2,
    /// anything between blanks; for button 3, a file name) around it.
    fn expand(&self, t: Tid, q0: usize, q1: usize, file: bool) -> String {
        let (q0, q1) = self.expand_range(t, q0, q1, file);
        self.text(t).map(|x| x.b.string(q0, q1)).unwrap_or_default()
    }

    /// Where the text [`Acme::expand`] finds is.
    fn expand_range(&self, t: Tid, q0: usize, q1: usize, file: bool) -> (usize, usize) {
        let Some(x) = self.text(t) else {
            return (q0, q1);
        };
        if q0 < q1 {
            return (q0, q1);
        }
        if x.q0 < x.q1 && q0 >= x.q0 && q0 <= x.q1 {
            return (x.q0, x.q1);
        }
        let ok = |c: char| {
            if file { is_file(c) } else { !c.is_whitespace() }
        };
        let (mut a, mut b) = (q0, q0);
        while a > 0 && ok(x.b.at(a - 1)) {
            a -= 1;
        }
        while b < x.b.len() && ok(x.b.at(b)) {
            b += 1;
        }
        (a, b)
    }

    /// A button 2 (`kind` `X`) or button 3 (`L`) action on `q0..q1` of
    /// `t`, when its window's `event` file is open: queues it as acme(4)
    /// events for the program reading them, and returns true. Otherwise
    /// returns false, for acme to act on it.
    fn send_event(&mut self, t: Tid, q0: usize, q1: usize, kind: char) -> bool {
        let Some(id) = Acme::win_of(t) else {
            return false;
        };
        if self.win(id).is_none_or(|w| w.fs.nevent == 0) {
            return false;
        }
        let (e0, e1) = self.expand_range(t, q0, q1, kind == 'L');
        let text = self.text(t).map(|x| x.b.string(e0, e1)).unwrap_or_default();
        let mut flag = 0;
        if kind == 'X' {
            let cmd = text.split_whitespace().next().unwrap_or("");
            if BUILTINS.contains(&cmd) {
                flag |= 1;
            }
        } else {
            let (name, _) = split_addr(&text);
            let path = self.resolve(&self.dir_of(t), name);
            if !name.is_empty() && !own(&path) && purgatorio::stat(&path).is_ok() {
                flag |= 4;
                if self.by_name(&path).is_some() {
                    flag |= 1;
                }
            } else {
                flag |= 1;
            }
        }
        let k = if matches!(t, Tid::Tag(_)) {
            kind.to_ascii_lowercase()
        } else {
            kind
        };
        let mut ev = String::new();
        if (e0, e1) != (q0, q1) {
            // A null selection with a non-null expansion: the click, then
            // what it expands to.
            ev.push_str(&fs::event('M', k, q0, q1, flag | 2, ""));
            ev.push_str(&fs::event('M', k, e0, e1, 0, &text));
        } else {
            ev.push_str(&fs::event('M', k, q0, q1, flag, &text));
        }
        if let Some(w) = self.win_mut(id) {
            w.fs.events.extend_from_slice(ev.as_bytes());
        }
        true
    }

    /// Turns the inserts and deletes made in windows whose `event` file
    /// is open into events from `origin`: `K` the keyboard, `M` the
    /// mouse, `E` writes to `body` or `tag`, `F` the other files.
    fn post_changes(&mut self, origin: char) {
        for w in self.cols.iter_mut().flat_map(|c| c.wins.iter_mut()) {
            let open = w.fs.nevent > 0;
            for (tag, b) in [(false, &mut w.body.b), (true, &mut w.tag.b)] {
                for c in b.changes.drain(..) {
                    if !open {
                        continue;
                    }
                    let kind = match (c.insert, tag) {
                        (true, false) => 'I',
                        (false, false) => 'D',
                        (true, true) => 'i',
                        (false, true) => 'd',
                    };
                    let text = c.text.unwrap_or_default();
                    let e = fs::event(origin, kind, c.q0, c.q1, 0, &text);
                    w.fs.events.extend_from_slice(e.as_bytes());
                }
            }
        }
    }

    /// Answers the reads of acme(4) files that wait, after anything acme
    /// did. A file system whose pipe broke is dropped.
    fn serve_waiting(&mut self) {
        if let Some(mut f) = self.fs.take() {
            match f.flush(self) {
                Ok(()) => self.fs = Some(f),
                Err(e) => eprintln!("acme: {}: {e}", fs::MTPT),
            }
        }
    }

    /// Answers the messages waiting on the file system's pipe.
    fn serve(&mut self) {
        if let Some(mut f) = self.fs.take() {
            match f.input(self) {
                Ok(true) => self.fs = Some(f),
                Ok(false) => {}
                Err(e) => eprintln!("acme: {}: {e}", fs::MTPT),
            }
        }
    }

    /// The scroll bar: button 1 moves the line at the top down to the
    /// pointer, button 3 moves the line at the pointer to the top, and
    /// button 2 shows the part of the file that far down.
    fn scrollbar(&mut self, id: u64, m: Pointer) -> Result<(), Error> {
        let fh = self.fh();
        let Some(w) = self.win(id) else {
            return Ok(());
        };
        let r = w.scrollr();
        let lines = ((m.xy.y - r.min.y) / fh).max(1);
        if m.buttons & 1 != 0 {
            self.scroll(id, -lines);
        } else if m.buttons & 4 != 0 {
            self.scroll(id, lines);
        } else if m.buttons & 2 != 0 {
            let len = w.body.b.len();
            let q = (len as i64 * (m.xy.y - r.min.y) as i64 / r.dy().max(1) as i64) as usize;
            if let Some((tx, f)) = self.text_font(Tid::Body(id)) {
                let width = tx.r.dx();
                tx.org = frame::wrapped_start(&tx.b, q.min(len), width, f);
                tx.dirty = true;
            }
        }
        self.track(|_, _| Ok(()))
    }

    // ---- Keyboard.

    fn key(&mut self, ch: char) -> Result<(), Error> {
        let t = match self.hit(self.m.xy) {
            Hit::Text(t) => t,
            Hit::Scroll(id) => Tid::Body(id),
            Hit::Box(id) => Tid::Tag(id),
            Hit::Nothing => return Ok(()),
        };
        let fh = self.fh();
        let Some(x) = self.text(t) else {
            return Ok(());
        };
        let lines = x.max_lines(fh).max(1) as i32;
        match ch {
            KUP | KDOWN | KPGUP | KPGDOWN => {
                if let Some(id) = Acme::win_of(t) {
                    let n = match ch {
                        KUP => -(lines / 3).max(1),
                        KDOWN => (lines / 3).max(1),
                        KPGUP => -lines.max(1),
                        _ => lines.max(1),
                    };
                    self.scroll(id, n);
                }
                return Ok(());
            }
            KHOME | KEND => {
                if let Some(x) = self.text_mut(t) {
                    let q = if ch == KHOME { 0 } else { x.b.len() };
                    x.org = 0;
                    x.dirty = true;
                    if ch == KEND {
                        self.show(t, q);
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        self.seltext = Some(t);
        let typing = self.typing.filter(|(tt, _)| *tt == t).map(|(_, q)| q);
        if ch == '\x1B' {
            // Escape cuts the selection, or selects what was just typed.
            if self.text(t).is_some_and(|x| x.q0 < x.q1) {
                self.cut(t, true)?;
            } else if let Some(q) = typing
                && let Some(x) = self.text_mut(t)
            {
                let q1 = x.q0;
                x.set_select(q.min(q1), q1);
            }
            self.typing = None;
            return self.edited(t);
        }
        // A window a program reads the events of gets the interrupt
        // keys too, for `win` to pass on.
        let events = matches!(t, Tid::Body(_))
            && Acme::win_of(t)
                .and_then(|id| self.win(id))
                .is_some_and(|w| w.fs.nevent > 0);
        let Some(x) = self.text_mut(t) else {
            return Ok(());
        };
        let mut new_typing = None;
        match ch {
            KLEFT => {
                let q = x.q0.saturating_sub(1);
                x.set_select(q, q);
            }
            KRIGHT => {
                let q = (x.q1 + 1).min(x.b.len());
                x.set_select(q, q);
            }
            '\x01' => {
                let q = frame::line_start(&x.b, x.q0);
                x.set_select(q, q);
            }
            '\x05' => {
                let mut q = x.q1;
                while q < x.b.len() && x.b.at(q) != '\n' {
                    q += 1;
                }
                x.set_select(q, q);
            }
            '\x08' | '\x15' | '\x17' => {
                if x.q0 < x.q1 {
                    x.replace("");
                } else {
                    let q1 = x.q0;
                    let mut q0 = q1;
                    match ch {
                        '\x08' => q0 = q0.saturating_sub(1),
                        '\x15' => {
                            q0 = frame::line_start(&x.b, q1);
                            if q0 == q1 {
                                q0 = q0.saturating_sub(1);
                            }
                        }
                        _ => {
                            while q0 > 0 && !is_word(x.b.at(q0 - 1)) && x.b.at(q0 - 1) != '\n' {
                                q0 -= 1;
                            }
                            while q0 > 0 && is_word(x.b.at(q0 - 1)) {
                                q0 -= 1;
                            }
                            if q0 == q1 {
                                q0 = q0.saturating_sub(1);
                            }
                        }
                    }
                    x.q0 = q0;
                    x.replace("");
                }
                new_typing = typing.map(|q| q.min(x.q0));
            }
            '\n' if x.kind == Kind::Tag => return Ok(()),
            c if c == '\n'
                || c == '\t'
                || !c.is_control()
                || (events && (c == '\x7F' || c == '\x03')) =>
            {
                let start = typing.unwrap_or(x.q0);
                x.replace(&c.to_string());
                new_typing = Some(start.min(x.q0));
            }
            _ => return Ok(()),
        }
        let (q, kind) = (x.q0, x.kind);
        self.typing = new_typing.map(|q| (t, q));
        if kind == Kind::Body {
            self.show(t, q);
        }
        self.edited(t)
    }

    /// After a text changes: a body's tag shows whether it is modified;
    /// a window's warning is forgotten.
    fn edited(&mut self, t: Tid) -> Result<(), Error> {
        self.touch(t);
        if let Tid::Body(id) = t {
            if let Some(w) = self.win_mut(id) {
                w.warned = false;
            }
            self.retag(id);
            self.exit_warned = false;
        }
        Ok(())
    }

    // ---- Snarf, cut and paste.

    /// Copies the selection of `t` to the snarf buffer and, if `del`,
    /// deletes it.
    fn cut(&mut self, t: Tid, del: bool) -> Result<(), Error> {
        let Some(x) = self.text_mut(t) else {
            return Ok(());
        };
        if x.q0 == x.q1 {
            return Ok(());
        }
        let s = x.selection();
        if del {
            x.replace("");
        }
        self.snarf = s;
        if del {
            self.edited(t)?;
        }
        Ok(())
    }

    /// Replaces the selection of `t` with the snarf buffer and selects
    /// it.
    fn paste(&mut self, t: Tid) -> Result<(), Error> {
        let s = self.snarf.clone();
        let Some(x) = self.text_mut(t) else {
            return Ok(());
        };
        let q0 = x.q0;
        x.replace(&s);
        let q1 = x.q0;
        x.set_select(q0, q1);
        self.edited(t)
    }

    /// The text `Snarf`, `Cut` and `Paste` act on when run from `t`: the
    /// selected text in the same window, or the window's body; from a
    /// row or column tag, the text last selected in.
    fn target(&self, t: Tid) -> Option<Tid> {
        match Acme::win_of(t) {
            Some(id) => match self.seltext {
                Some(s) if Acme::win_of(s) == Some(id) => Some(s),
                _ => Some(Tid::Body(id)),
            },
            None => self.seltext,
        }
    }

    /// The window `Undo`, `Put` and the like act on from `t`.
    fn target_win(&self, t: Tid) -> Option<u64> {
        Acme::win_of(t).or_else(|| self.seltext.and_then(Acme::win_of))
    }

    // ---- Messages.

    /// Appends `msg` to the `+Errors` window of directory `dir` (the
    /// current directory when empty), making it if needed.
    fn error(&mut self, dir: &str, msg: &str) {
        let dir = if dir.is_empty() {
            self.cwd.clone()
        } else {
            dir.to_string()
        };
        let name = if dir == "/" {
            String::from("/+Errors")
        } else {
            format!("{dir}/+Errors")
        };
        let id = match self.by_name(&name) {
            Some(id) => id,
            None => {
                let ci = self.cols.len().saturating_sub(1);
                self.new_win(ci, &name, "", false)
            }
        };
        let mut msg = msg.to_string();
        if !msg.is_empty() && !msg.ends_with('\n') {
            msg.push('\n');
        }
        if let Some(w) = self.win_mut(id) {
            let n = w.body.b.len();
            w.body.b.insert(n, &msg);
            let n = w.body.b.len();
            w.body.set_select(n, n);
        }
        let n = self.text(Tid::Body(id)).map_or(0, |x| x.b.len());
        self.show(Tid::Body(id), n);
        self.touch(Tid::Body(id));
        self.retag(id);
    }

    // ---- Files.

    /// Reads file or directory `name` for a body.
    fn read(name: &str) -> Result<(String, bool), Error> {
        if own(name) {
            return Err(Error::from(EOWN));
        }
        let d = purgatorio::stat(name)?;
        if d.qid.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(name)
                .map_err(|_| Error(purgatorio::errstr()))?
                .filter_map(|e| e.ok())
                .map(|e| {
                    let mut n = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        n.push('/');
                    }
                    n
                })
                .collect();
            names.sort();
            let mut s = names.join("\n");
            if !s.is_empty() {
                s.push('\n');
            }
            return Ok((s, true));
        }
        let data = std::fs::read(name).map_err(|_| Error(purgatorio::errstr()))?;
        Ok((String::from_utf8_lossy(&data).into_owned(), false))
    }

    /// Opens `name` in a new window in column `ci`, or shows the window
    /// that has it; selects line `line` if given.
    fn open(&mut self, ci: usize, name: &str, line: Option<usize>) -> u64 {
        let id = match self.by_name(name) {
            Some(id) => id,
            None => match Acme::read(name) {
                Ok((s, dir)) => {
                    let n = if dir && !name.ends_with('/') {
                        format!("{name}/")
                    } else {
                        name.to_string()
                    };
                    self.new_win(ci, &n, &s, dir)
                }
                // A file that does not exist yet opens empty, to be Put.
                Err(_) => self.new_win(ci, name, "", false),
            },
        };
        if let Some(line) = line {
            self.goto_line(id, line);
        }
        id
    }

    fn goto_line(&mut self, id: u64, line: usize) {
        let Some(w) = self.win_mut(id) else {
            return;
        };
        let b = &w.body.b;
        let mut q0 = 0;
        let mut n = 1;
        while n < line && q0 < b.len() {
            if b.at(q0) == '\n' {
                n += 1;
            }
            q0 += 1;
        }
        let mut q1 = q0;
        while q1 < b.len() && b.at(q1) != '\n' {
            q1 += 1;
        }
        if q1 < b.len() {
            q1 += 1;
        }
        w.body.set_select(q0, q1);
        self.show(Tid::Body(id), q0);
    }

    fn get(&mut self, id: u64, arg: &str) {
        let Some(w) = self.win(id) else {
            return;
        };
        let dir = w.dir(&self.cwd);
        if w.modified() && !w.warned && arg.is_empty() {
            let n = w.name();
            self.error(&dir, &format!("{n} modified"));
            if let Some(w) = self.win_mut(id) {
                w.warned = true;
            }
            return;
        }
        let name = if arg.is_empty() {
            w.name()
        } else {
            self.resolve(&dir, arg)
        };
        if name.is_empty() {
            return;
        }
        match Acme::read(&name) {
            Ok((s, is_dir)) => {
                if let Some(w) = self.win_mut(id) {
                    w.body.b.load(&s);
                    w.body.set_select(0, 0);
                    w.body.org = 0;
                    w.is_dir = is_dir;
                    w.warned = false;
                    let n = if is_dir && !name.ends_with('/') {
                        format!("{name}/")
                    } else {
                        name
                    };
                    set_tag(w, &n);
                }
                self.touch(Tid::Body(id));
                self.touch(Tid::Tag(id));
            }
            Err(e) => self.error(&dir, &format!("{name}: {e}")),
        }
    }

    fn put(&mut self, id: u64, arg: &str) {
        let Some(w) = self.win(id) else {
            return;
        };
        let dir = w.dir(&self.cwd);
        let name = if arg.is_empty() {
            w.name()
        } else {
            self.resolve(&dir, arg)
        };
        if name.is_empty() {
            self.error(&dir, "no file name");
            return;
        }
        if w.is_dir {
            self.error(&dir, &format!("{name} is a directory"));
            return;
        }
        if own(&name) {
            self.error(&dir, &format!("{name}: {EOWN}"));
            return;
        }
        let s = w.body.b.all();
        match std::fs::write(&name, s.as_bytes()) {
            Ok(()) => {
                if let Some(w) = self.win_mut(id) {
                    w.body.b.set_clean();
                    w.warned = false;
                    set_tag(w, &name);
                    w.tag.dirty = true;
                }
            }
            Err(_) => {
                let e = purgatorio::errstr();
                self.error(&dir, &format!("{name}: {e}"));
            }
        }
    }

    fn resolve(&self, dir: &str, name: &str) -> String {
        if name.starts_with('/') {
            clean(name)
        } else {
            clean(&format!("{dir}/{name}"))
        }
    }

    // ---- Commands.

    fn execute(&mut self, t: Tid, s: &str) -> Result<(), Error> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(());
        }
        let (cmd, arg) = match s.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (s, ""),
        };
        let win = self.target_win(t);
        if let Some(id) = win
            && let Some(w) = self.win_mut(id)
        {
            w.body.b.mark();
        }
        match cmd {
            "Exit" => {
                let modified: Vec<String> = self
                    .cols
                    .iter()
                    .flat_map(|c| c.wins.iter())
                    .filter(|w| w.modified())
                    .map(Window::name)
                    .collect();
                if modified.is_empty() || self.exit_warned {
                    self.exit = true;
                } else {
                    for n in modified {
                        self.error("", &format!("{n} modified"));
                    }
                    self.exit_warned = true;
                }
            }
            "Newcol" => self.new_col(),
            "Delcol" => {
                let ci = self.col_of(t);
                if ci < self.cols.len() {
                    self.del_col(ci);
                }
            }
            "New" => {
                let ci = self.col_of(t);
                let id = self.new_win(ci, "", "", false);
                self.seltext = Some(Tid::Body(id));
            }
            "Del" | "Delete" => {
                if let Some(id) = Acme::win_of(t) {
                    let Some(w) = self.win(id) else {
                        return Ok(());
                    };
                    if cmd == "Del" && w.modified() && !w.warned {
                        let (n, dir) = (w.name(), w.dir(&self.cwd));
                        self.error(&dir, &format!("{n} modified"));
                        if let Some(w) = self.win_mut(id) {
                            w.warned = true;
                        }
                    } else {
                        self.del_win(id);
                    }
                }
            }
            "Get" => {
                if let Some(id) = win {
                    self.get(id, arg);
                }
            }
            "Put" => {
                if let Some(id) = win {
                    self.put(id, arg);
                }
            }
            "Putall" => {
                let ids: Vec<u64> = self
                    .cols
                    .iter()
                    .flat_map(|c| c.wins.iter())
                    .filter(|w| w.modified())
                    .map(|w| w.id)
                    .collect();
                for id in ids {
                    self.put(id, "");
                }
            }
            "Undo" | "Redo" => {
                if let Some(id) = win
                    && let Some(w) = self.win_mut(id)
                {
                    let r = if cmd == "Undo" {
                        w.body.b.undo()
                    } else {
                        w.body.b.redo()
                    };
                    if let Some((q0, q1)) = r {
                        w.body.set_select(q0, q1);
                        w.body.fix_org();
                        self.show(Tid::Body(id), q0);
                    }
                    self.edited(Tid::Body(id))?;
                }
            }
            "Snarf" | "Cut" => {
                if let Some(x) = self.target(t) {
                    self.cut(x, cmd == "Cut")?;
                }
            }
            "Paste" => {
                if let Some(x) = self.target(t) {
                    self.paste(x)?;
                }
            }
            "Look" => {
                let what = if arg.is_empty() {
                    self.target(t)
                        .and_then(|x| self.text(x))
                        .map(Text::selection)
                        .unwrap_or_default()
                } else {
                    arg.to_string()
                };
                if let Some(id) = win {
                    self.search(id, &what);
                }
            }
            _ => self.run(t, s)?,
        }
        if let Some(id) = win {
            self.retag(id);
        }
        Ok(())
    }

    /// Runs command line `s` with sh in the directory of `t`; its output
    /// goes to that directory's `+Errors` window.
    fn run(&mut self, t: Tid, s: &str) -> Result<(), Error> {
        let dir = self.dir_of(t);
        let [r, w] = purgatorio::pipe()?;
        let null = purgatorio::open("/dev/null", purgatorio::abi::mode::OREAD)?;
        // As in acme(1), a command run from a window knows it as $winid.
        let winid = Acme::win_of(t).map_or_else(String::new, |id| format!("winid={id}; "));
        let argv = vec![
            String::from("sh"),
            String::from("-c"),
            format!("{winid}cd {}; {}", quote(&dir), s),
        ];
        let map = [
            (null.as_raw_fd(), 0),
            (w.as_raw_fd(), 1),
            (w.as_raw_fd(), 2),
        ];
        // Each command gets a copy of acme's namespace, with /mnt/acme,
        // and a process group of its own.
        let flags = pctl::NEWFD | pctl::FORKNS | pctl::NEWPGRP;
        if let Err(e) = purgatorio::spawn("sh", &argv, flags, &map) {
            self.error(&dir, &format!("{s}: {e}"));
            return Ok(());
        }
        drop(w);
        drop(null);
        self.children.push(Child {
            fd: File::from(r),
            dir,
            carry: Vec::new(),
        });
        Ok(())
    }

    /// Output from child `i`.
    fn output(&mut self, i: usize) -> Result<(), Error> {
        let mut buf = [0u8; 8192];
        let n = self.children[i].fd.read(&mut buf).unwrap_or(0);
        if n == 0 {
            let c = self.children.remove(i);
            if !c.carry.is_empty() {
                let s = String::from_utf8_lossy(&c.carry).into_owned();
                self.append(&c.dir, &s);
            }
            return Ok(());
        }
        let c = &mut self.children[i];
        c.carry.extend_from_slice(&buf[..n]);
        let valid = match std::str::from_utf8(&c.carry) {
            Ok(s) => s.len(),
            Err(e) if e.error_len().is_some() => c.carry.len(),
            Err(e) => e.valid_up_to(),
        };
        let s = String::from_utf8_lossy(&c.carry[..valid]).into_owned();
        c.carry.drain(..valid);
        let dir = c.dir.clone();
        self.append(&dir, &s);
        Ok(())
    }

    /// Appends output to a directory's `+Errors` window without adding a
    /// newline.
    fn append(&mut self, dir: &str, s: &str) {
        if s.is_empty() {
            return;
        }
        let name = if dir == "/" {
            String::from("/+Errors")
        } else {
            format!("{dir}/+Errors")
        };
        if self.by_name(&name).is_none() {
            self.error(dir, "");
        }
        let Some(id) = self.by_name(&name) else {
            return;
        };
        if let Some(w) = self.win_mut(id) {
            let n = w.body.b.len();
            w.body.b.insert(n, s);
            let n = w.body.b.len();
            w.body.set_select(n, n);
        }
        let n = self.text(Tid::Body(id)).map_or(0, |x| x.b.len());
        self.show(Tid::Body(id), n);
        self.touch(Tid::Body(id));
        self.retag(id);
    }

    /// Button 3: opens the file `s` names, relative to the directory of
    /// `t`, or else searches for `s` in the window's body.
    fn look(&mut self, t: Tid, s: &str) -> Result<(), Error> {
        if s.is_empty() {
            return Ok(());
        }
        let dir = self.dir_of(t);
        let (name, line) = split_addr(s);
        if name.is_empty()
            && let (Some(line), Some(id)) = (line, Acme::win_of(t))
        {
            self.goto_line(id, line);
            return Ok(());
        }
        if !name.is_empty() {
            let path = self.resolve(&dir, name);
            if !own(&path) && purgatorio::stat(&path).is_ok() {
                let ci = self.col_of(t);
                let id = self.open(ci, &path, line);
                self.seltext = Some(Tid::Body(id));
                return Ok(());
            }
        }
        if let Some(id) = Acme::win_of(t) {
            self.search(id, s);
        }
        Ok(())
    }

    /// Selects the next `s` after the selection in body `id`, wrapping
    /// round, and moves the pointer to it.
    fn search(&mut self, id: u64, s: &str) {
        let pat: Vec<char> = s.chars().collect();
        if pat.is_empty() {
            return;
        }
        let Some(w) = self.win(id) else {
            return;
        };
        let b = &w.body.b;
        let n = b.len();
        let matches =
            |q: usize| q + pat.len() <= n && (0..pat.len()).all(|i| b.at(q + i) == pat[i]);
        let start = w.body.q1;
        let found = (start..n).chain(0..start).find(|&q| matches(q));
        let Some(q) = found else {
            return;
        };
        if let Some(w) = self.win_mut(id) {
            w.body.set_select(q, q + pat.len());
        }
        self.seltext = Some(Tid::Body(id));
        self.show(Tid::Body(id), q);
        // Lay it out now, to know where to put the pointer.
        let _ = self.redraw();
        let fh = self.fh();
        if let Some((tx, f)) = self.text_font(Tid::Body(id))
            && let Some(i) = frame::line_of(&tx.lines, q)
        {
            let l = tx.lines[i];
            let r = tx.r;
            let x = r.min.x + frame::x_of(&tx.b, &l, q, f) + 2;
            let y = r.min.y + i as i32 * fh + fh / 2;
            let _ = self.mouse.set(Point::new(x, y));
        }
    }

    // ---- The loop.

    fn main_loop(&mut self) -> Result<(), Error> {
        while !self.exit {
            self.serve_waiting();
            self.redraw()?;
            if self.kbd.pending() {
                let c = self.kbd.read()?;
                self.key(c)?;
                self.post_changes('K');
                continue;
            }
            let nchild = self.children.len();
            let mut fds = vec![Ready::Read(self.mouse.fd()), Ready::Read(self.kbd.fd())];
            fds.extend(self.children.iter().map(|c| Ready::Read(c.fd.as_raw_fd())));
            let wait_i = self.waitfd.as_ref().map(|w| {
                fds.push(Ready::Read(w.as_raw_fd()));
                fds.len() - 1
            });
            let fs_i = self.fs.as_ref().map(|f| {
                fds.push(Ready::Read(f.fd()));
                fds.len() - 1
            });
            let Some(i) = purgatorio::ready(&fds, -1)? else {
                continue;
            };
            match i {
                0 => {
                    let m = self.mouse.read()?;
                    self.mouse(m)?;
                    self.post_changes('M');
                }
                1 => {
                    let c = self.kbd.read()?;
                    self.key(c)?;
                    self.post_changes('K');
                }
                i if i >= 2 && i - 2 < nchild => {
                    self.output(i - 2)?;
                    self.post_changes('F');
                }
                i if Some(i) == fs_i => self.serve(),
                i if Some(i) == wait_i => {
                    let mut buf = [0u8; 512];
                    let n = self
                        .waitfd
                        .as_mut()
                        .map_or(0, |w| w.read(&mut buf).unwrap_or(0));
                    if n == 0 {
                        self.waitfd = None;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Writes a window's tag: its name, the commands that apply now, a bar,
/// and whatever the user typed after the bar.
fn set_tag(w: &mut Window, name: &str) {
    let old = w.tag.b.all();
    let (bar, user) = match old.find('|') {
        Some(i) => (old[..i].chars().count(), old[i + 1..].to_string()),
        None => (old.chars().count(), String::from(" Look ")),
    };
    let mut s = format!("{name} Del Snarf");
    if w.body.b.can_undo() {
        s.push_str(" Undo");
    }
    if w.body.b.can_redo() {
        s.push_str(" Redo");
    }
    if w.modified() {
        s.push_str(" Put");
    }
    s.push_str(" |");
    let newbar = s.chars().count() - 1;
    s.push_str(&user);
    if s == old {
        return;
    }
    let shift = |q: usize| {
        if q > bar {
            (q + newbar).saturating_sub(bar)
        } else {
            q.min(newbar)
        }
    };
    let (q0, q1) = (shift(w.tag.q0), shift(w.tag.q1));
    // acme's own rewriting of the tag is not an event.
    let watch = std::mem::replace(&mut w.tag.b.watch, false);
    let n = w.tag.b.len();
    w.tag.b.delete(0, n);
    w.tag.b.insert(0, &s);
    w.tag.b.watch = watch;
    w.tag.set_select(q0, q1);
    w.tag.dirty = true;
}

/// A double click at `q`: the line when at either end of one, the text
/// between brackets or quotes when next to one, else the word.
fn double_click(b: &text::Buffer, q: usize) -> (usize, usize) {
    let n = b.len();
    let prev = if q > 0 { Some(b.at(q - 1)) } else { None };
    let next = if q < n { Some(b.at(q)) } else { None };
    if prev.is_none_or(|c| c == '\n') || next.is_none_or(|c| c == '\n') {
        // The whole line, with its newline.
        let s = frame::line_start(
            b,
            q.saturating_sub(usize::from(
                next.is_none_or(|c| c == '\n') && prev.is_some_and(|c| c != '\n'),
            )),
        );
        let mut e = s;
        while e < n && b.at(e) != '\n' {
            e += 1;
        }
        if e < n {
            e += 1;
        }
        return (s, e);
    }
    for (open, close) in [
        ('(', ')'),
        ('[', ']'),
        ('{', '}'),
        ('<', '>'),
        ('"', '"'),
        ('\'', '\''),
    ] {
        if prev == Some(open) {
            let mut depth = 0;
            for e in q..n {
                let c = b.at(e);
                if c == close && depth == 0 {
                    return (q, e);
                }
                if c == open && open != close {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                }
            }
        }
        if next == Some(close) {
            let mut depth = 0;
            for s in (0..q).rev() {
                let c = b.at(s);
                if c == open && depth == 0 {
                    return (s + 1, q);
                }
                if c == close && open != close {
                    depth += 1;
                } else if c == open {
                    depth -= 1;
                }
            }
        }
    }
    let (mut s, mut e) = (q, q);
    while s > 0 && is_word(b.at(s - 1)) {
        s -= 1;
    }
    while e < n && is_word(b.at(e)) {
        e += 1;
    }
    (s, e)
}

use purgatorio::draw::Rect;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--mount") {
        return fs::mount_helper();
    }
    let mut args = args.into_iter().skip(1).peekable();
    let mut font = None;
    while let Some(a) = args.peek() {
        if a == "-f" {
            args.next();
            font = Some(
                args.next()
                    .ok_or_else(|| Error::from("usage: acme [-f font] [file ...]"))?,
            );
        } else if a == "--" {
            args.next();
            break;
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(Error::from("usage: acme [-f font] [file ...]"));
        } else {
            break;
        }
    }
    let font_name = font.clone().unwrap_or_else(|| {
        purgatorio::env::get("font")
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| String::from(purgatorio::draw::DEFAULT_FONT))
    });
    let font = match font {
        Some(f) => Font::open(&f)?,
        None => Font::default_font()?,
    };
    let mut a = Acme::new(font, font_name)?;
    a.fs = fs::start(&mut a);
    let files: Vec<String> = args.collect();
    for f in &files {
        let path = a.resolve(&a.cwd.clone(), f);
        a.open(0, &path, None);
    }
    if let Some(id) = a.cols.first().and_then(|c| c.wins.first()).map(|w| w.id) {
        a.seltext = Some(Tid::Body(id));
    }
    a.main_loop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(clean("/a/./b//c/../d"), "/a/b/d");
        assert_eq!(clean("/.."), "/");
        assert_eq!(clean("a/../../b"), "../b");
        assert_eq!(split_addr("f.rs:12"), ("f.rs", Some(12)));
        assert_eq!(split_addr("f.rs:"), ("f.rs", None));
        assert_eq!(split_addr(":3"), ("", Some(3)));
        assert_eq!(quote("it's"), "'it''s'");
    }

    #[test]
    fn double_clicks() {
        let mut b = text::Buffer::new(false);
        b.insert(0, "one two(x y)\nnext\n");
        assert_eq!(double_click(&b, 5), (4, 7));
        assert_eq!(double_click(&b, 8), (8, 11));
        assert_eq!(double_click(&b, 11), (8, 11));
        assert_eq!(double_click(&b, 0), (0, 13));
        assert_eq!(double_click(&b, 12), (0, 13));
        assert_eq!(double_click(&b, 13), (13, 18));
    }
}
