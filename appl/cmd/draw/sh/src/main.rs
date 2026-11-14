//! draw/sh: a shell window drawn on `/dev/draw`, after Inferno's `wm/sh`
//! and the text windows of Plan 9's rio, with no toolkit.
//!
//! `draw/sh [-f font] [-c command]` starts `sh -i` in a process group
//! of its own with its standard input, output and error on pipes, and
//! fills the screen (or its window) with a scroll bar and the text.
//! What the shell writes is put in at the output point, which then moves
//! past it; text typed after the output point is the shell's input,
//! sent a line at a time when Enter is typed. `-c command` is sent as
//! the first line once the shell has prompted.
//!
//! Keys: Backspace, `^U` (all the typed input), `^W` (a word), `^D` (the
//! input without a newline, or the end of input when nothing is typed),
//! Delete (`interrupt` to the shell's process group), up and down arrows
//! and Page Up and Page Down to scroll, Home and End.
//!
//! The pointer: button 1 selects; button 2 is a menu of `cut`, `paste`,
//! `snarf` and `send` (as rio's: `send` types the selection, or the
//! snarf buffer, as input); the wheel scrolls three lines; in the scroll
//! bar, button 1 scrolls back by the lines above the pointer, button 3
//! forward, and button 2 jumps to that point in the text. The snarf
//! buffer is `/dev/snarf` (`#^`), shared with rio, acme and the host.
//!
//! The window ends when the shell does, killing the shell's process
//! group. When the window itself is killed, a watcher (`draw/sh -W pid`,
//! a second copy holding the read end of a pipe only the window can
//! write) sees the pipe close and kills the shell's group, so nothing is
//! left behind.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::pctl;
use purgatorio::arg::Arg;
use purgatorio::draw::{Display, Font, Image, Keyboard, Mouse, Point, Pointer, Rect};
use purgatorio::{Context, Error, Ready};

const KHOME: char = '\u{F00D}';
const KUP: char = '\u{F00E}';
const KPGUP: char = '\u{F00F}';
const KPGDOWN: char = '\u{F013}';
const KEND: char = '\u{F018}';
const KDOWN: char = '\u{F800}';

/// The scroll bar's width and the margin round the text.
const BAR: i32 = 12;
const MARGIN: i32 = 4;
/// The most text kept; older lines are dropped beyond it.
const MAXTEXT: usize = 256 * 1024;
const MENU: [&str; 4] = ["cut", "paste", "snarf", "send"];

/// Writes `msg` to `/prog/<pid>/ctl`.
fn progctl(pid: u32, msg: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .write(true)
        .open(format!("/prog/{pid}/ctl"))
    {
        let _ = f.write_all(msg.as_bytes());
    }
}

/// The snarf buffer, `/dev/snarf`.
struct Snarf;

impl Snarf {
    fn get(&self) -> String {
        std::fs::read("/dev/snarf")
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    fn put(&mut self, s: String) {
        if let Ok(mut f) = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open("/dev/snarf")
        {
            let _ = f.write_all(s.as_bytes());
        }
    }
}

struct Colours {
    back: Image,
    text: Image,
    bar: Image,
    thumb: Image,
    high: Image,
    menu: Image,
    menuhi: Image,
    border: Image,
}

/// The text and the shell.
struct Term {
    d: Display,
    font: Font,
    widths: HashMap<char, i32>,
    c: Colours,
    text: Vec<char>,
    /// The output point.
    q: usize,
    /// The first character shown, the start of a line.
    org: usize,
    sel: (usize, usize),
    snarf: Snarf,
    sh: u32,
    to_sh: Option<File>,
    from_sh: File,
    carry: Vec<u8>,
    /// A command to send once the shell first prompts.
    first: Option<String>,
}

impl Term {
    fn screen(&self) -> Rect {
        self.d.screen().r
    }

    fn textr(&self) -> Rect {
        let r = self.screen();
        Rect::new(
            r.min.x + BAR + MARGIN,
            r.min.y + MARGIN,
            r.max.x - MARGIN,
            r.max.y - MARGIN,
        )
    }

    fn rows(&self) -> usize {
        (self.textr().dy() / self.font.height).max(1) as usize
    }

    fn width(&mut self, c: char) -> i32 {
        if let Some(&w) = self.widths.get(&c) {
            return w;
        }
        let w = self.font.string_width(c.encode_utf8(&mut [0; 4]));
        self.widths.insert(c, w);
        w
    }

    /// How far `c` advances from `x`: tabs go to the next multiple of
    /// four widths of `0`.
    fn advance(&mut self, c: char, x: i32) -> i32 {
        if c == '\t' {
            let tab = (4 * self.width('0')).max(1);
            return (x / tab + 1) * tab - x;
        }
        self.width(c)
    }

    /// The starts of the wrapped lines from `from` (a line start): at
    /// most `max + 1` of them, none at or past `stop`.
    fn wrap(&mut self, from: usize, max: usize, stop: usize) -> Vec<usize> {
        let width = self.textr().dx().max(1);
        let stop = stop.min(self.text.len());
        let mut starts = vec![from];
        let (mut x, mut i) = (0, from);
        while i < stop && starts.len() <= max {
            let c = self.text[i];
            if c == '\n' {
                i += 1;
                if i < stop || i == self.text.len() {
                    starts.push(i);
                }
                x = 0;
                continue;
            }
            let w = self.advance(c, x);
            if x > 0 && x + w > width {
                starts.push(i);
                x = 0;
                continue;
            }
            x += w;
            i += 1;
        }
        starts
    }

    /// The start of the logical line holding `p`.
    fn line_start(&self, p: usize) -> usize {
        self.text[..p]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1)
    }

    /// The line start `n` wrapped lines before line start `q`.
    fn back(&mut self, mut q: usize, mut n: usize) -> usize {
        while n > 0 && q > 0 {
            let ls = self.line_start(q - 1);
            let starts = self.wrap(ls, usize::MAX - 1, q);
            let starts: Vec<usize> = starts.into_iter().filter(|&s| s < q).collect();
            if starts.len() >= n {
                return starts[starts.len() - n];
            }
            n -= starts.len();
            q = ls;
        }
        q
    }

    /// The wrapped lines on screen, with the start after the last.
    fn shown(&mut self) -> Vec<usize> {
        let rows = self.rows();
        self.wrap(self.org, rows, usize::MAX)
    }

    fn end_visible(&mut self) -> bool {
        self.shown().len() <= self.rows()
    }

    /// Scrolls so that the end of the text is on the last line.
    fn show_end(&mut self) {
        let len = self.text.len();
        let ls = self.line_start(len);
        let last = *self.wrap(ls, usize::MAX - 1, len + 1).last().unwrap_or(&ls);
        let rows = self.rows();
        self.org = self.back(last, rows - 1);
    }

    fn scroll(&mut self, n: i32) {
        if n < 0 {
            self.org = self.back(self.org, n.unsigned_abs() as usize);
        } else {
            let s = self.wrap(self.org, n as usize, usize::MAX);
            let i = (n as usize).min(s.len() - 1);
            self.org = s[i];
        }
    }

    /// Drops old text beyond [`MAXTEXT`], a line at a time.
    fn trim(&mut self) {
        if self.text.len() <= MAXTEXT {
            return;
        }
        let want = self.text.len() - MAXTEXT * 3 / 4;
        let cut = self.text[want..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(want, |i| want + i + 1)
            .min(self.q);
        self.text.drain(..cut);
        self.q -= cut;
        self.org = self.org.saturating_sub(cut);
        self.org = self.line_start(self.org);
        self.sel = (
            self.sel.0.saturating_sub(cut),
            self.sel.1.saturating_sub(cut),
        );
    }

    /// Output from the shell, put in at the output point.
    fn output(&mut self, s: &str) {
        let follow = self.end_visible();
        let add: Vec<char> = s.chars().filter(|&c| c != '\r' && c != '\0').collect();
        let n = add.len();
        let q = self.q;
        self.text.splice(q..q, add);
        self.q += n;
        if self.sel.0 >= q {
            self.sel.0 += n;
        }
        if self.sel.1 >= q {
            self.sel.1 += n;
        }
        self.trim();
        if follow {
            self.show_end();
        }
    }

    /// Sends the typed input up to and including its last newline.
    fn send(&mut self) {
        let typed = &self.text[self.q..];
        let Some(nl) = typed.iter().rposition(|&c| c == '\n') else {
            return;
        };
        let line: String = typed[..=nl].iter().collect();
        self.q += nl + 1;
        if let Some(f) = &mut self.to_sh {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Types `c` at the end of the text.
    fn key(&mut self, c: char) {
        match c {
            '\x08' => {
                if self.text.len() > self.q {
                    self.text.pop();
                }
            }
            '\x15' => self.text.truncate(self.q),
            '\x17' => {
                let mut n = self.text.len();
                while n > self.q && self.text[n - 1].is_whitespace() {
                    n -= 1;
                }
                while n > self.q && !self.text[n - 1].is_whitespace() {
                    n -= 1;
                }
                self.text.truncate(n);
            }
            '\x04' => {
                if self.text.len() == self.q {
                    // The end of input: the shell sees its pipe close.
                    self.to_sh = None;
                } else {
                    let s: String = self.text[self.q..].iter().collect();
                    self.q = self.text.len();
                    if let Some(f) = &mut self.to_sh {
                        let _ = f.write_all(s.as_bytes());
                    }
                }
            }
            '\x7F' => progctl(self.sh, "interruptgrp"),
            KUP => self.scroll(-((self.rows() / 3).max(1) as i32)),
            KDOWN => self.scroll((self.rows() / 3).max(1) as i32),
            KPGUP => self.scroll(-(self.rows().saturating_sub(1).max(1) as i32)),
            KPGDOWN => self.scroll(self.rows().saturating_sub(1).max(1) as i32),
            KHOME => self.org = 0,
            KEND => self.show_end(),
            c if c == '\n'
                || c == '\t'
                || (!c.is_control() && !('\u{F000}'..='\u{F8FF}').contains(&c)) =>
            {
                self.text.push(c);
                if c == '\n' {
                    self.send();
                }
            }
            _ => {}
        }
        let len = self.text.len();
        self.sel = (self.sel.0.min(len), self.sel.1.min(len));
        self.trim();
        if !matches!(c, KUP | KDOWN | KPGUP | KPGDOWN | KHOME) && !self.end_visible() {
            self.show_end();
        }
    }

    /// The character position nearest point `p`.
    fn charof(&mut self, p: Point) -> usize {
        let tr = self.textr();
        let s = self.shown();
        let lines = s.len().min(self.rows());
        let row = ((p.y - tr.min.y) / self.font.height).max(0) as usize;
        // Below the last line counts as past the end of it.
        let px = if row >= lines { i32::MAX } else { p.x };
        let start = s[row.min(lines - 1)];
        let end = s
            .get(row.min(lines - 1) + 1)
            .copied()
            .unwrap_or(self.text.len());
        let (mut x, mut i) = (0, start);
        while i < end && self.text[i] != '\n' {
            let w = self.advance(self.text[i], x);
            if tr.min.x.saturating_add(x + w / 2) > px {
                break;
            }
            x += w;
            i += 1;
        }
        i
    }

    fn redraw(&mut self) -> Result<(), Error> {
        let screen = self.d.screen();
        let r = screen.r;
        let tr = self.textr();
        let zero = Point::new(0, 0);
        let fh = self.font.height;
        self.d.draw(&screen, r, &self.c.back, None, zero)?;
        // The scroll bar and its thumb: the part of the text shown.
        let bar = Rect::new(r.min.x, r.min.y, r.min.x + BAR, r.max.y);
        self.d.draw(&screen, bar, &self.c.bar, None, zero)?;
        let s = self.shown();
        let rows = self.rows();
        let len = self.text.len().max(1) as i64;
        let shown_end = s.get(rows).copied().unwrap_or(self.text.len());
        let h = i64::from(bar.dy());
        let y0 = bar.min.y + (self.org as i64 * h / len) as i32;
        let y1 = (bar.min.y + (shown_end as i64 * h / len) as i32).max(y0 + 2);
        let thumb = Rect::new(bar.min.x, y0, bar.max.x - 1, y1.min(bar.max.y));
        self.d.draw(&screen, thumb, &self.c.thumb, None, zero)?;
        let (s0, s1) = (self.sel.0.min(self.sel.1), self.sel.0.max(self.sel.1));
        let mut cursor = None;
        for row in 0..rows.min(s.len()) {
            let start = s[row];
            let end = s.get(row + 1).copied().unwrap_or(self.text.len());
            let y = tr.min.y + row as i32 * fh;
            // x of each position on the line.
            let mut xs = Vec::with_capacity(end - start + 1);
            let mut x = 0;
            for i in start..end {
                xs.push(x);
                let c = self.text[i];
                if c != '\n' {
                    x += self.advance(c, x);
                }
            }
            xs.push(x);
            if s1 > s0 && s0 <= end && s1 > start {
                let xa = tr.min.x + xs[s0.max(start) - start];
                let newline = end > start && self.text[end - 1] == '\n';
                let xb = if s1 >= end && newline {
                    tr.max.x
                } else {
                    tr.min.x + xs[s1.min(end) - start]
                };
                let hr = Rect::new(xa, y, xb.max(xa + 1), y + fh);
                self.d.draw(&screen, hr, &self.c.high, None, zero)?;
            }
            // The runes, a run at a time between tabs.
            let mut i = start;
            while i < end {
                let mut j = i;
                while j < end && self.text[j] != '\t' && self.text[j] != '\n' {
                    j += 1;
                }
                if j > i {
                    let run: String = self.text[i..j].iter().collect();
                    let p = Point::new(tr.min.x + xs[i - start], y);
                    self.d
                        .string(&screen, p, &self.c.text, zero, &mut self.font, &run)?;
                }
                i = j + 1;
            }
            // The cursor is at the end of the text, on the last line
            // when the end is shown.
            if s.len() <= rows && row + 1 == s.len() {
                cursor = Some(Point::new(tr.min.x + xs[end - start], y));
            }
        }
        if let Some(p) = cursor {
            let tick = Rect::new(p.x, p.y + 1, p.x + 2, p.y + fh - 1);
            self.d.draw(&screen, tick, &self.c.text, None, zero)?;
        }
        self.d.flush()
    }

    /// Button 1 down at `p`: selects while it is held.
    fn select(&mut self, mouse: &mut Mouse, p: Point) -> Result<u32, Error> {
        let q0 = self.charof(p);
        self.sel = (q0, q0);
        self.redraw()?;
        loop {
            let m = mouse.read()?;
            let q1 = self.charof(m.xy);
            if q1 != self.sel.1 {
                self.sel.1 = q1;
                self.redraw()?;
            }
            if m.buttons & 1 == 0 {
                let (a, b) = self.sel;
                self.sel = (a.min(b), a.max(b));
                return Ok(m.buttons);
            }
        }
    }

    /// The button 2 menu at `p`, held until the button is released;
    /// returns the item chosen and the buttons then.
    fn menu(&mut self, mouse: &mut Mouse, p: Point) -> Result<(Option<usize>, u32), Error> {
        let fh = self.font.height + 4;
        let w = MENU
            .iter()
            .map(|s| self.font.string_width(s))
            .max()
            .unwrap_or(0)
            + 16;
        let r = self.screen();
        let x0 = (p.x - w / 2).clamp(r.min.x, (r.max.x - w).max(r.min.x));
        let y0 = (p.y - fh / 2).clamp(r.min.y, (r.max.y - fh * MENU.len() as i32).max(r.min.y));
        let item = |q: Point| -> Option<usize> {
            if q.x < x0 || q.x >= x0 + w || q.y < y0 {
                return None;
            }
            let i = ((q.y - y0) / fh) as usize;
            (i < MENU.len()).then_some(i)
        };
        let mut hi = item(p);
        loop {
            let screen = self.d.screen();
            let zero = Point::new(0, 0);
            let box_r = Rect::new(x0 - 1, y0 - 1, x0 + w + 1, y0 + fh * MENU.len() as i32 + 1);
            self.d.draw(&screen, box_r, &self.c.border, None, zero)?;
            for (i, s) in MENU.iter().enumerate() {
                let ir = Rect::new(x0, y0 + i as i32 * fh, x0 + w, y0 + (i as i32 + 1) * fh);
                let bg = if hi == Some(i) {
                    &self.c.menuhi
                } else {
                    &self.c.menu
                };
                self.d.draw(&screen, ir, bg, None, zero)?;
                let tw = self.font.string_width(s);
                let at = Point::new(x0 + (w - tw) / 2, ir.min.y + 2);
                self.d
                    .string(&screen, at, &self.c.text, zero, &mut self.font, s)?;
            }
            self.d.flush()?;
            let m = mouse.read()?;
            hi = item(m.xy);
            if m.buttons & 2 == 0 {
                return Ok((if m.buttons == 0 { hi } else { None }, m.buttons));
            }
        }
    }

    fn selected(&self) -> String {
        self.text[self.sel.0..self.sel.1].iter().collect()
    }

    fn do_menu(&mut self, item: usize) {
        match MENU[item] {
            "cut" | "snarf" => {
                if self.sel.1 > self.sel.0 {
                    let s = self.selected();
                    self.snarf.put(s);
                    // Only typed input can be cut.
                    if MENU[item] == "cut" && self.sel.0 >= self.q {
                        self.text.drain(self.sel.0..self.sel.1);
                        self.sel = (self.sel.0, self.sel.0);
                    }
                }
            }
            "paste" => {
                for c in self.snarf.get().chars() {
                    self.key(c);
                }
            }
            "send" => {
                if self.sel.1 > self.sel.0 {
                    let s = self.selected();
                    self.snarf.put(s);
                }
                let mut s = self.snarf.get();
                if !s.ends_with('\n') {
                    s.push('\n');
                }
                for c in s.chars() {
                    self.key(c);
                }
            }
            _ => {}
        }
    }

    /// Acts on pointer event `m`, the buttons having been `prev`, and
    /// returns the buttons held after.
    fn pointer(&mut self, mouse: &mut Mouse, m: Pointer, prev: u32) -> Result<u32, Error> {
        let pressed = m.buttons & !prev;
        if pressed & 8 != 0 {
            self.scroll(-3);
            return Ok(m.buttons);
        }
        if pressed & 16 != 0 {
            self.scroll(3);
            return Ok(m.buttons);
        }
        let r = self.screen();
        if m.xy.x < r.min.x + BAR {
            let lines = ((m.xy.y - self.textr().min.y) / self.font.height).max(1);
            if pressed & 1 != 0 {
                self.scroll(-lines);
            } else if pressed & 4 != 0 {
                self.scroll(lines);
            } else if pressed & 2 != 0 {
                let p =
                    self.text.len() as i64 * i64::from(m.xy.y - r.min.y) / i64::from(r.dy().max(1));
                self.org = self.line_start((p.max(0) as usize).min(self.text.len()));
            }
            return Ok(m.buttons);
        }
        if pressed & 1 != 0 {
            return self.select(mouse, m.xy);
        }
        if pressed & 2 != 0 {
            let (item, b) = self.menu(mouse, m.xy)?;
            if let Some(i) = item {
                self.do_menu(i);
            }
            return Ok(b);
        }
        Ok(m.buttons)
    }

    /// Reads what the shell wrote; false at its end.
    fn read_shell(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        let n = self.from_sh.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return false;
        }
        self.carry.extend_from_slice(&buf[..n]);
        let valid = match std::str::from_utf8(&self.carry) {
            Ok(s) => s.len(),
            Err(e) if e.error_len().is_some() => self.carry.len(),
            Err(e) => e.valid_up_to(),
        };
        let s = String::from_utf8_lossy(&self.carry[..valid]).into_owned();
        self.carry.drain(..valid);
        self.output(&s);
        if let Some(cmd) = self.first.take() {
            for c in cmd.chars().chain(std::iter::once('\n')) {
                self.key(c);
            }
        }
        true
    }
}

/// `draw/sh -W pid`: waits for its standard input, a pipe only the
/// window writes, to close, then kills `pid`'s process group.
fn watch(pid: &str) -> Result<(), Error> {
    let pid: u32 = pid.parse().map_err(|_| Error::from("bad pid"))?;
    let mut buf = [0u8; 64];
    let mut stdin = std::io::stdin();
    while matches!(stdin.read(&mut buf), Ok(n) if n > 0) {}
    progctl(pid, "killgrp");
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("-W") {
        return watch(args.get(2).map_or("", String::as_str));
    }
    let me = args
        .first()
        .cloned()
        .unwrap_or_else(|| String::from("draw/sh"));
    let mut arg = Arg::new(args, "draw/sh [-f font] [-c command]");
    let (mut font, mut first) = (None, None);
    while let Some(c) = arg.opt() {
        match c {
            'f' => font = Some(arg.earg()?),
            'c' => first = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error::from("usage: draw/sh [-f font] [-c command]"));
    }
    // Fds of our own, so that the pipes close when this prog goes.
    purgatorio::pctl(pctl::FORKFD, &[])?;
    let font = match font {
        Some(f) => Font::open(&f)?,
        None => Font::default_font()?,
    };
    let mut d = Display::open()?;
    let c = Colours {
        back: d.color(0xFFFFFFFF)?,
        text: d.color(0x000000FF)?,
        bar: d.color(0x9EA0A6FF)?,
        thumb: d.color(0xE6E6E6FF)?,
        high: d.color(0xEEEE9EFF)?,
        menu: d.color(0xEAFFEAFF)?,
        menuhi: d.color(0x88CC88FF)?,
        border: d.color(0x448844FF)?,
    };
    let mut mouse = Mouse::open()?;
    let mut kbd = Keyboard::open()?;

    let [in_r, in_w] = purgatorio::pipe()?;
    let [out_r, out_w] = purgatorio::pipe()?;
    let sh = purgatorio::spawn(
        "/bin/sh.wasm",
        &[String::from("sh"), String::from("-i")],
        pctl::NEWFD | pctl::FORKNS | pctl::NEWPGRP,
        &[
            (in_r.as_raw_fd(), 0),
            (out_w.as_raw_fd(), 1),
            (out_w.as_raw_fd(), 2),
        ],
    )?;
    drop((in_r, out_w));
    let [life_r, life_w] = purgatorio::pipe()?;
    let path = purgatorio::find_command(&me).unwrap_or_else(|_| String::from("/bin/draw/sh.wasm"));
    if let Err(e) = purgatorio::spawn(
        &path,
        &[me.clone(), String::from("-W"), sh.to_string()],
        pctl::NEWFD,
        &[(life_r.as_raw_fd(), 0)],
    ) {
        progctl(sh, "killgrp");
        return Err(e);
    }
    drop(life_r);
    let mut t = Term {
        d,
        font,
        widths: HashMap::new(),
        c,
        text: Vec::new(),
        q: 0,
        org: 0,
        sel: (0, 0),
        snarf: Snarf,
        sh,
        to_sh: Some(File::from(in_w)),
        from_sh: File::from(out_r),
        carry: Vec::new(),
        first,
    };
    let r = t.run(&mut mouse, &mut kbd);
    progctl(sh, "killgrp");
    drop(life_w);
    r
}

impl Term {
    fn run(&mut self, mouse: &mut Mouse, kbd: &mut Keyboard) -> Result<(), Error> {
        self.redraw()?;
        let mut buttons = 0;
        loop {
            if kbd.pending() {
                let c = kbd.read()?;
                self.key(c);
                self.redraw()?;
                continue;
            }
            let fds = [
                Ready::Read(self.from_sh.as_raw_fd()),
                Ready::Read(mouse.fd()),
                Ready::Read(kbd.fd()),
            ];
            match purgatorio::ready(&fds, 1000)? {
                Some(0) => {
                    if !self.read_shell() {
                        return Ok(());
                    }
                    // Take what else is waiting before drawing.
                    while purgatorio::ready(&fds[..1], 0)? == Some(0) {
                        if !self.read_shell() {
                            return Ok(());
                        }
                    }
                }
                Some(1) => {
                    let m = mouse.read()?;
                    if self.d.check_resize()? {
                        self.show_end();
                    }
                    buttons = self.pointer(mouse, m, buttons)?;
                }
                Some(_) => {
                    let c = kbd.read()?;
                    self.key(c);
                }
                None => {
                    // A window manager may resize without a pointer
                    // event; look now and then.
                    if !self.d.check_resize()? {
                        continue;
                    }
                    self.show_end();
                }
            }
            self.redraw()?;
        }
    }
}
