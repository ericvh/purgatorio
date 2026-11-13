//! rio: a window system on `/dev/draw`, after Plan 9's rio(1) and rio(4).
//!
//! `rio [-f font] [-i cmd]` takes the screen (or, run in a rio window,
//! that window), fills it with a background and serves windows on it.
//! Button 3 on the background, on a window's border or in a text window
//! brings up the menu: `New`, `Resize`, `Move`, `Delete`, `Hide`, `Exit`,
//! then the hidden windows by label. `New` and `Resize` ask for a
//! rectangle, swept with button 3; `Resize`, `Move`, `Delete` and `Hide`
//! ask for a window, chosen with button 3 (`Move` drags it). Button 1 on
//! a window that is not current makes it current and raises it. `-i cmd`
//! runs `cmd` in a first window that fills most of the screen.
//!
//! Each window runs `sh -i` with the window's files (rio(4), see
//! [`fs`]) bound before `/dev`. Without a program drawing in it, a window
//! is a simple terminal: what is written to `/dev/cons` is shown, typing
//! goes at the end and is sent a line at a time, `^D` sends what is
//! typed without a newline, Delete flushes what is typed and interrupts
//! the window's processes, and the wheel scrolls. A program that opens
//! `/dev/draw` gets the window, not the screen (Plan 9's `getwindow`, in
//! the SDK, through `/dev/winname`), and while it holds `/dev/pointer`
//! open the pointer's events in the window go to it.
//!
//! How draw is shared: the draw device has Plan 9's screens, windows and
//! named images. rio makes a public screen on its image and one window
//! on it per rio window, and names it; the program in the window looks
//! the name up and makes a screen and window of its own on it. A resize
//! or move gives the window a new image and a new name, and the program
//! hears of it as a pointer event marked `r`. See docs/graphics.md.
//!
//! `rio -W [cmd]` is the window side of a new window, run by rio: it
//! mounts the window's files (fd 0) before `/dev` in a namespace of its
//! own and runs `cmd` with `sh -c`, or `sh -i`, on `/dev/cons`.

mod fs;
mod text;

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::{mount, pctl};
use purgatorio::draw::{
    BORDER, Display, Font, Image, Keyboard, Mouse, Point, Pointer, Rect, Screen, WHITE, inset,
};
use purgatorio::{Context, Error, Ready};

use fs::{EDELETED, Session, Wctl};
use text::{Text, Widths};

const BACKGROUND: u32 = 0x777777FF;
/// The border of the current window, and of the others.
const FOCUS: u32 = 0x2E6E6EFF;
const UNFOCUS: u32 = 0x9ECECEFF;
const MENUBG: u32 = 0xEAFFEAFF;
const MENUHI: u32 = 0x448844FF;
const SWEEP: u32 = 0xDD2222FF;
/// The smallest window a sweep makes.
const MINSIZE: i32 = 50;
/// Pointer events kept for a program that does not read them.
const MOUSEQ: usize = 64;

/// The menu's fixed items; hidden windows follow.
pub const MENU: &[&str] = &["New", "Resize", "Move", "Delete", "Hide", "Exit"];
/// Pixels above and below each menu item's text.
pub const MENUPAD: i32 = 2;

pub struct Win {
    pub id: u32,
    /// The window's rectangle on the screen, border included.
    pub r: Rect,
    img: Image,
    /// The name of `img` (`/dev/winname`).
    pub name: String,
    /// Images the window has had, for new names.
    serial: u32,
    pub label: String,
    pub hidden: bool,
    /// The window's prog (`rio -W`), the leader of its process group.
    pid: u32,
    pub sess: Session,
    pub text: Text,
    /// Scroll to show the end before the next drawing.
    pub follow: bool,
    /// The text needs drawing.
    dirty: bool,
    pub mouseq: VecDeque<String>,
    pub mouse_opens: u32,
    pub kbdq: Vec<u8>,
    pub kbd_opens: u32,
    pub rawq: Vec<u8>,
    pub raw: bool,
    pub consctl_opens: u32,
    /// Stacking order: higher is nearer the top.
    z: u64,
}

struct Colours {
    black: Image,
    white: Image,
    focus: Image,
    unfocus: Image,
    menubg: Image,
    menuhi: Image,
}

pub struct Rio {
    d: Display,
    screen: Image,
    scr: Screen,
    c: Colours,
    widths: Widths,
    pub mouse: Mouse,
    kbd: Keyboard,
    m: Pointer,
    pub wins: Vec<Win>,
    pub current: Option<u32>,
    next_id: u32,
    z: u64,
    lasthit: usize,
    /// The window that gets the pointer while the buttons pressed in it
    /// are held.
    grab: Option<u32>,
    waitfd: Option<File>,
    pub user: String,
    exit: bool,
}

fn progctl(pid: u32, msg: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .write(true)
        .open(format!("/prog/{pid}/ctl"))
    {
        let _ = f.write_all(msg.as_bytes());
    }
}

/// The rectangle with corners `a` and `b`.
fn canon(a: Point, b: Point) -> Rect {
    Rect::new(a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y))
}

impl Rio {
    fn new(font: Font) -> Result<Rio, Error> {
        let mut d = Display::open()?;
        let screen = d.screen();
        let bg = d.color(BACKGROUND)?;
        d.draw(&screen, screen.r, &bg, None, Point::new(0, 0))?;
        let scr = d.alloc_screen(&screen, &bg, true)?;
        let c = Colours {
            black: d.color(0x000000FF)?,
            white: d.color(WHITE)?,
            focus: d.color(FOCUS)?,
            unfocus: d.color(UNFOCUS)?,
            menubg: d.color(MENUBG)?,
            menuhi: d.color(MENUHI)?,
        };
        d.flush()?;
        let user = std::fs::read_to_string("/dev/user").unwrap_or_else(|_| String::from("rio"));
        Ok(Rio {
            d,
            screen,
            scr,
            c,
            widths: Widths::new(font),
            mouse: Mouse::open()?,
            kbd: Keyboard::open()?,
            m: Pointer::default(),
            wins: Vec::new(),
            current: None,
            next_id: 1,
            z: 0,
            lasthit: 0,
            grab: None,
            waitfd: File::open(format!("/prog/{}/wait", purgatorio::pid())).ok(),
            user,
            exit: false,
        })
    }

    pub fn win(&self, id: u32) -> Option<&Win> {
        self.wins.iter().find(|w| w.id == id)
    }

    pub fn win_mut(&mut self, id: u32) -> Option<&mut Win> {
        self.wins.iter_mut().find(|w| w.id == id)
    }

    /// The visible window at `p`, the topmost, and whether `p` is inside
    /// its border.
    fn at(&self, p: Point) -> Option<(u32, bool)> {
        self.wins
            .iter()
            .filter(|w| !w.hidden && w.r.contains(p))
            .max_by_key(|w| w.z)
            .map(|w| (w.id, inset(w.r, BORDER).contains(p)))
    }

    /// The text of window `id` needs drawing again.
    pub fn redraw(&mut self, id: u32) {
        if let Some(w) = self.win_mut(id) {
            w.dirty = true;
        }
    }

    // ---- Drawing.

    fn border(&mut self, id: u32) -> Result<(), Error> {
        let cur = self.current == Some(id);
        let Some(w) = self.win(id) else {
            return Ok(());
        };
        let (img, r) = (w.img.clone(), w.r);
        let col = if cur { &self.c.focus } else { &self.c.unfocus };
        let b = BORDER;
        for side in [
            Rect::new(r.min.x, r.min.y, r.max.x, r.min.y + b),
            Rect::new(r.min.x, r.max.y - b, r.max.x, r.max.y),
            Rect::new(r.min.x, r.min.y, r.min.x + b, r.max.y),
            Rect::new(r.max.x - b, r.min.y, r.max.x, r.max.y),
        ] {
            self.d.draw(&img, side, col, None, Point::new(0, 0))?;
        }
        Ok(())
    }

    /// Draws window `id`'s text, unless a program draws in it.
    fn paint(&mut self, id: u32) -> Result<(), Error> {
        let Some(w) = self.wins.iter_mut().find(|w| w.id == id) else {
            return Ok(());
        };
        w.dirty = false;
        if w.mouse_opens > 0 {
            return Ok(());
        }
        let area = Text::area(w.r, BORDER);
        let fh = self.widths.font.height.max(1);
        let n = (area.dy() / fh).max(1) as usize;
        let width = area.dx().max(1);
        let f = &mut self.widths;
        if w.follow {
            w.follow = false;
            if !w.text.end_shows(n, width, f) {
                w.text.org = w.text.org_for_end(n, width, f);
            }
        }
        let lines = w.text.layout(w.text.org, n, width, f);
        let mut runs = Vec::new();
        for (i, &(s, e)) in lines.iter().enumerate() {
            for (t, x) in w.text.pieces(s, e, f) {
                runs.push((t, Point::new(area.min.x + x, area.min.y + i as i32 * fh)));
            }
        }
        let tick = w.text.end_point(&lines, fh, f);
        let img = w.img.clone();
        let inner = inset(w.r, BORDER);
        self.d
            .draw(&img, inner, &self.c.white, None, Point::new(0, 0))?;
        for (t, p) in runs {
            self.d.string(
                &img,
                p,
                &self.c.black,
                Point::new(0, 0),
                &mut self.widths.font,
                &t,
            )?;
        }
        if let Some(p) = tick {
            let p = Point::new(area.min.x + p.x, area.min.y + p.y);
            let r = Rect::new(p.x, p.y, p.x + 1, p.y + fh);
            self.d
                .draw(&img, r, &self.c.black, None, Point::new(0, 0))?;
            for y in [p.y, p.y + fh - 1] {
                let r = Rect::new(p.x - 1, y, p.x + 2, y + 1);
                self.d
                    .draw(&img, r, &self.c.black, None, Point::new(0, 0))?;
            }
        }
        Ok(())
    }

    // ---- Windows.

    /// A new window on `r` running `cmd` (`sh -i` when empty).
    fn new_window(&mut self, r: Rect, cmd: &str) -> Result<u32, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let img = self.d.alloc_window(&self.scr, r, WHITE)?;
        let name = format!("window.{}.{id}.1", purgatorio::pid());
        if let Err(e) = self.d.name_image(&img, &name, true) {
            self.d.free(img)?;
            return Err(e);
        }
        let [ours, theirs] = purgatorio::pipe()?;
        let mut argv = vec![String::from("rio"), String::from("-W")];
        if !cmd.is_empty() {
            argv.push(String::from(cmd));
        }
        let pid = match purgatorio::spawn(
            "/bin/rio.wasm",
            &argv,
            pctl::NEWFD | pctl::FORKNS | pctl::NEWPGRP,
            &[(theirs.as_raw_fd(), 0), (2, 2)],
        ) {
            Ok(p) => p,
            Err(e) => {
                self.d.name_image(&img, &name, false)?;
                self.d.free(img)?;
                return Err(e);
            }
        };
        drop(theirs);
        self.z += 1;
        self.wins.push(Win {
            id,
            r,
            img,
            name,
            serial: 1,
            label: format!("window {id}"),
            hidden: false,
            pid,
            sess: Session::new(File::from(ours)),
            text: Text::default(),
            follow: true,
            dirty: true,
            mouseq: VecDeque::new(),
            mouse_opens: 0,
            kbdq: Vec::new(),
            kbd_opens: 0,
            rawq: Vec::new(),
            raw: false,
            consctl_opens: 0,
            z: self.z,
        });
        self.focus(id)?;
        Ok(id)
    }

    /// Makes window `id` current and raises it.
    fn focus(&mut self, id: u32) -> Result<(), Error> {
        let old = self.current.replace(id);
        self.z += 1;
        let z = self.z;
        if let Some(w) = self.win_mut(id) {
            w.z = z;
            let img = w.img.clone();
            self.d.top_windows(&[&img], true)?;
        }
        if let Some(o) = old
            && o != id
        {
            self.border(o)?;
        }
        self.border(id)
    }

    /// Makes the topmost visible window current, if any.
    fn focus_top(&mut self) -> Result<(), Error> {
        self.current = None;
        if let Some(id) = self
            .wins
            .iter()
            .filter(|w| !w.hidden)
            .max_by_key(|w| w.z)
            .map(|w| w.id)
        {
            self.focus(id)?;
        }
        Ok(())
    }

    /// Gives window `id` a new image on `r`, with a new name; with
    /// `keep`, the old pixels moved with it. A program drawing in it
    /// hears of it.
    fn reshape(&mut self, id: u32, r: Rect, keep: bool) -> Result<(), Error> {
        let pid = purgatorio::pid();
        let scr = self.scr.clone();
        let Some(w) = self.win_mut(id) else {
            return Ok(());
        };
        let (old, oldname) = (w.img.clone(), w.name.clone());
        w.serial += 1;
        let name = format!("window.{pid}.{id}.{}", w.serial);
        let img = self.d.alloc_window(&scr, r, WHITE)?;
        if keep {
            self.d.draw(&img, r, &old, None, old.r.min)?;
        }
        self.d.name_image(&img, &name, true)?;
        self.d.name_image(&old, &oldname, false)?;
        self.d.free(old)?;
        let m = self.m;
        let Some(w) = self.win_mut(id) else {
            return Ok(());
        };
        w.img = img;
        w.r = r;
        w.name = name;
        w.hidden = false;
        w.dirty = true;
        w.follow = true;
        if w.mouse_opens > 0 {
            Rio::queue_mouse(w, m, 'r');
        }
        self.focus(id)
    }

    fn hide(&mut self, id: u32) -> Result<(), Error> {
        let far = Point::new(self.screen.r.max.x + 64, self.screen.r.max.y + 64);
        let Some(w) = self.win_mut(id) else {
            return Ok(());
        };
        if w.hidden {
            return Ok(());
        }
        w.hidden = true;
        let img = w.img.clone();
        self.d.origin_window(&img, far)?;
        if self.current == Some(id) {
            self.focus_top()?;
        }
        Ok(())
    }

    fn unhide(&mut self, id: u32) -> Result<(), Error> {
        let Some(w) = self.win_mut(id) else {
            return Ok(());
        };
        if w.hidden {
            w.hidden = false;
            let img = w.img.clone();
            self.d.origin_window(&img, img.r.min)?;
        }
        self.focus(id)
    }

    /// Kills window `id`'s processes and takes it away.
    fn delete(&mut self, id: u32) -> Result<(), Error> {
        if let Some(w) = self.win(id) {
            progctl(w.pid, "killgrp");
        }
        self.remove(id)
    }

    /// Takes window `id` away; its files hang up.
    fn remove(&mut self, id: u32) -> Result<(), Error> {
        let Some(i) = self.wins.iter().position(|w| w.id == id) else {
            return Ok(());
        };
        let w = self.wins.remove(i);
        if self.grab == Some(id) {
            self.grab = None;
        }
        self.d.name_image(&w.img, &w.name, false)?;
        self.d.free(w.img)?;
        if self.current == Some(id) {
            self.focus_top()?;
        }
        Ok(())
    }

    /// A rectangle for a window made without one.
    fn default_rect(&self) -> Rect {
        let s = self.screen.r;
        let (w, h) = ((s.dx() * 2 / 3).max(MINSIZE), (s.dy() * 2 / 3).max(MINSIZE));
        let off = 20 * (self.wins.len() as i32 % 5);
        let min = Point::new(s.min.x + s.dx() / 6 + off, s.min.y + s.dy() / 6 + off);
        Rect::new(min.x, min.y, min.x + w, min.y + h)
    }

    /// A wctl request written by window `id`'s programs.
    pub fn wctl(&mut self, id: u32, c: Wctl) -> Result<(), String> {
        let r = self.win(id).ok_or(EDELETED)?.r;
        let ok = |r: Rect| -> Result<Rect, String> {
            if r.dx() < 2 * BORDER + 4 || r.dy() < 2 * BORDER + 4 {
                Err(String::from("bad rectangle in wctl request"))
            } else {
                Ok(r)
            }
        };
        let e = |e: Error| e.0;
        match c.verb.as_str() {
            "new" => {
                let nr = ok(c.r.unwrap_or_else(|| self.default_rect()))?;
                self.new_window(nr, &c.cmd).map(drop).map_err(e)
            }
            "resize" => {
                let mut nr = c.r.unwrap_or(r);
                nr.max.x += c.dx.unwrap_or(0);
                nr.max.y += c.dy.unwrap_or(0);
                self.reshape(id, ok(nr)?, false).map_err(e)
            }
            "move" => {
                let p = Point::new(c.minx.unwrap_or(r.min.x), c.miny.unwrap_or(r.min.y));
                self.reshape(id, r.sub(r.min).add(p), true).map_err(e)
            }
            "top" => self.focus(id).map_err(e),
            "current" => self.focus(id).map_err(e),
            "bottom" => {
                let img = self.win(id).ok_or(EDELETED)?.img.clone();
                if let Some(w) = self.win_mut(id) {
                    w.z = 0;
                }
                self.d.top_windows(&[&img], false).map_err(e)
            }
            "hide" => self.hide(id).map_err(e),
            "unhide" => self.unhide(id).map_err(e),
            "delete" => self.delete(id).map_err(e),
            "scroll" | "noscroll" => Ok(()),
            _ => Err(String::from("unrecognized wctl command")),
        }
    }

    // ---- The pointer.

    fn queue_mouse(w: &mut Win, m: Pointer, kind: char) {
        if w.mouseq.len() >= MOUSEQ {
            w.mouseq.pop_front();
        }
        w.mouseq.push_back(format!(
            "{kind}{:11} {:11} {:11} {:11} ",
            m.xy.x, m.xy.y, m.buttons, m.msec
        ));
    }

    fn read_mouse(&mut self) -> Result<Pointer, Error> {
        self.m = self.mouse.read()?;
        Ok(self.m)
    }

    /// Waits for the buttons to be released.
    fn release(&mut self) -> Result<(), Error> {
        while self.m.buttons != 0 {
            self.read_mouse()?;
        }
        Ok(())
    }

    fn mouse_event(&mut self, m: Pointer) -> Result<(), Error> {
        let pressed = m.buttons & !self.m.buttons;
        self.m = m;
        if let Some(g) = self.grab {
            if let Some(w) = self.win_mut(g) {
                Rio::queue_mouse(w, m, 'm');
            }
            if m.buttons == 0 {
                self.grab = None;
            }
            return Ok(());
        }
        let hit = self.at(m.xy);
        let drawn = |rio: &Rio, id: u32| rio.win(id).is_some_and(|w| w.mouse_opens > 0);
        if pressed & 4 != 0 {
            let to_rio = match hit {
                None => true,
                Some((id, inner)) => !inner || !drawn(self, id),
            };
            if to_rio {
                return self.menu();
            }
        }
        if pressed & 1 != 0
            && let Some((id, _)) = hit
            && self.current != Some(id)
        {
            self.focus(id)?;
            return self.release();
        }
        if let Some((id, true)) = hit
            && self.current == Some(id)
            && drawn(self, id)
        {
            if let Some(w) = self.win_mut(id) {
                Rio::queue_mouse(w, m, 'm');
            }
            if pressed != 0 {
                self.grab = Some(id);
            }
            return Ok(());
        }
        if pressed & (8 | 16) != 0
            && let Some((id, _)) = hit
            && !drawn(self, id)
        {
            let n = if pressed & 8 != 0 { -3 } else { 3 };
            let width = self
                .win(id)
                .map_or(1, |w| Text::area(w.r, BORDER).dx().max(1));
            let f = &mut self.widths;
            if let Some(w) = self.wins.iter_mut().find(|w| w.id == id) {
                w.text.scroll(n, width, f);
                w.dirty = true;
            }
        }
        Ok(())
    }

    /// The menu, from a button 3 press: shows it until the button is
    /// released and does what was chosen.
    fn menu(&mut self) -> Result<(), Error> {
        let hidden: Vec<(u32, String)> = self
            .wins
            .iter()
            .filter(|w| w.hidden)
            .map(|w| (w.id, w.label.clone()))
            .collect();
        let items: Vec<String> = MENU
            .iter()
            .map(|s| s.to_string())
            .chain(hidden.iter().map(|h| h.1.clone()))
            .collect();
        let hit = self.menuhit(&items)?;
        let Some(i) = hit else {
            return Ok(());
        };
        match items[i].as_str() {
            _ if i >= MENU.len() => self.unhide(hidden[i - MENU.len()].0),
            "New" => {
                if let Some(r) = self.sweep()?
                    && let Err(e) = self.new_window(r, "")
                {
                    eprintln!("rio: new window: {e}");
                }
                Ok(())
            }
            "Resize" => {
                if let Some(id) = self.pick()?
                    && let Some(r) = self.sweep()?
                {
                    self.reshape(id, r, false)?;
                }
                Ok(())
            }
            "Move" => self.drag(),
            "Delete" => match self.pick()? {
                Some(id) => self.delete(id),
                None => Ok(()),
            },
            "Hide" => match self.pick()? {
                Some(id) => self.hide(id),
                None => Ok(()),
            },
            "Exit" => {
                for w in &self.wins {
                    progctl(w.pid, "killgrp");
                }
                self.exit = true;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Where menu item `i` of `n` is, for a menu `r`.
    fn item_rect(r: Rect, ih: i32, i: usize) -> Rect {
        let y = r.min.y + 1 + i as i32 * ih;
        Rect::new(r.min.x + 1, y, r.max.x - 1, y + ih)
    }

    /// Shows `items` in a menu with the last item chosen under the
    /// pointer, and returns the item the pointer is on when button 3 is
    /// released.
    fn menuhit(&mut self, items: &[String]) -> Result<Option<usize>, Error> {
        let ih = self.widths.font.height + 2 * MENUPAD;
        let wid = items
            .iter()
            .map(|s| self.widths.font.string_width(s))
            .max()
            .unwrap_or(0)
            + 16;
        let last = self.lasthit.min(items.len() - 1);
        let p = self.m.xy;
        let h = ih * items.len() as i32 + 2;
        let s = self.screen.r;
        let mut x = p.x - wid / 2;
        let mut y = p.y - 1 - last as i32 * ih - ih / 2;
        x = x.clamp(s.min.x, (s.max.x - wid).max(s.min.x));
        y = y.clamp(s.min.y, (s.max.y - h).max(s.min.y));
        let r = Rect::new(x, y, x + wid, y + h);
        let mw = self.d.alloc_window(&self.scr, r, MENUBG)?;
        let hitp = |p: Point| -> Option<usize> {
            (0..items.len()).find(|&i| Rio::item_rect(r, ih, i).contains(p))
        };
        let mut hit = hitp(p);
        let mut shown = None;
        loop {
            if shown != Some(hit) {
                self.d
                    .draw(&mw, r, &self.c.menuhi, None, Point::new(0, 0))?;
                self.d
                    .draw(&mw, inset(r, 1), &self.c.menubg, None, Point::new(0, 0))?;
                for (i, t) in items.iter().enumerate() {
                    let ir = Rio::item_rect(r, ih, i);
                    let fg = if hit == Some(i) {
                        self.d
                            .draw(&mw, ir, &self.c.menuhi, None, Point::new(0, 0))?;
                        &self.c.menubg
                    } else {
                        &self.c.black
                    };
                    let tw = self.widths.font.string_width(t);
                    let tp = Point::new(ir.min.x + (ir.dx() - tw) / 2, ir.min.y + MENUPAD);
                    self.d
                        .string(&mw, tp, fg, Point::new(0, 0), &mut self.widths.font, t)?;
                }
                self.d.flush()?;
                shown = Some(hit);
            }
            if self.m.buttons & 4 == 0 {
                break;
            }
            let m = self.read_mouse()?;
            hit = hitp(m.xy);
        }
        self.d.free(mw)?;
        self.d.flush()?;
        self.release()?;
        if let Some(i) = hit {
            self.lasthit = i;
        }
        Ok(hit)
    }

    /// Draws a box round `r` in thin windows, replacing `old`.
    fn outline(&mut self, old: Vec<Image>, r: Option<Rect>) -> Result<Vec<Image>, Error> {
        for i in old {
            self.d.free(i)?;
        }
        let mut out = Vec::new();
        if let Some(r) = r {
            let t = 2;
            for side in [
                Rect::new(r.min.x, r.min.y, r.max.x, r.min.y + t),
                Rect::new(r.min.x, r.max.y - t, r.max.x, r.max.y),
                Rect::new(r.min.x, r.min.y, r.min.x + t, r.max.y),
                Rect::new(r.max.x - t, r.min.y, r.max.x, r.max.y),
            ] {
                if side.dx() > 0 && side.dy() > 0 {
                    out.push(self.d.alloc_window(&self.scr, side, SWEEP)?);
                }
            }
        }
        self.d.flush()?;
        Ok(out)
    }

    /// Waits for a button press; true when it is button 3 alone.
    fn press3(&mut self) -> Result<bool, Error> {
        while self.m.buttons == 0 {
            self.read_mouse()?;
        }
        if self.m.buttons != 4 {
            self.release()?;
            return Ok(false);
        }
        Ok(true)
    }

    /// A rectangle swept with button 3.
    fn sweep(&mut self) -> Result<Option<Rect>, Error> {
        if !self.press3()? {
            return Ok(None);
        }
        let p0 = self.m.xy;
        let mut boxes = Vec::new();
        let mut r = canon(p0, p0);
        while self.m.buttons == 4 {
            let m = self.read_mouse()?;
            r = canon(p0, m.xy);
            boxes = self.outline(boxes, Some(r))?;
        }
        let ok = self.m.buttons == 0;
        self.outline(boxes, None)?;
        self.release()?;
        if !ok || r.dx() < MINSIZE || r.dy() < MINSIZE {
            return Ok(None);
        }
        Ok(Some(r))
    }

    /// A window chosen with button 3.
    fn pick(&mut self) -> Result<Option<u32>, Error> {
        if !self.press3()? {
            return Ok(None);
        }
        let hit = self.at(self.m.xy).map(|h| h.0);
        self.release()?;
        Ok(hit)
    }

    /// A window dragged with button 3 to a new place.
    fn drag(&mut self) -> Result<(), Error> {
        if !self.press3()? {
            return Ok(());
        }
        let p0 = self.m.xy;
        let Some((id, _)) = self.at(p0) else {
            return self.release();
        };
        let r0 = self.win(id).map_or(Rect::new(0, 0, 0, 0), |w| w.r);
        let mut boxes = Vec::new();
        let mut r = r0;
        while self.m.buttons == 4 {
            let m = self.read_mouse()?;
            r = r0.add(m.xy - p0);
            boxes = self.outline(boxes, Some(r))?;
        }
        let ok = self.m.buttons == 0;
        self.outline(boxes, None)?;
        self.release()?;
        if ok && r != r0 {
            self.reshape(id, r, true)?;
        }
        Ok(())
    }

    // ---- The keyboard.

    fn key(&mut self, c: char) {
        let Some(id) = self.current else {
            return;
        };
        let Some(w) = self.win_mut(id) else {
            return;
        };
        let mut b = [0u8; 4];
        let bytes = c.encode_utf8(&mut b).as_bytes();
        if w.kbd_opens > 0 {
            w.kbdq.extend_from_slice(bytes);
        } else if w.raw {
            w.rawq.extend_from_slice(bytes);
        } else if c == '\x7F' {
            w.text.flush_input();
            w.dirty = true;
            progctl(w.pid, "interruptgrp");
        } else if !('\u{E000}'..='\u{F8FF}').contains(&c) {
            w.text.typed(c);
            w.follow = true;
            w.dirty = true;
        }
    }

    // ---- The loop.

    /// A child exited: a window's prog, whose window goes.
    fn waited(&mut self) -> Result<(), Error> {
        let mut b = [0u8; 256];
        let n = match self.waitfd.as_mut().map(|f| f.read(&mut b)) {
            Some(Ok(n)) if n > 0 => n,
            _ => {
                self.waitfd = None;
                return Ok(());
            }
        };
        let s = String::from_utf8_lossy(&b[..n]);
        if let Some(pid) = s
            .split_whitespace()
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            && let Some(id) = self.wins.iter().find(|w| w.pid == pid).map(|w| w.id)
        {
            self.remove(id)?;
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        loop {
            self.serve_reads();
            let dirty: Vec<u32> = self.wins.iter().filter(|w| w.dirty).map(|w| w.id).collect();
            for id in dirty {
                self.paint(id)?;
            }
            self.d.flush()?;
            if self.exit {
                return Ok(());
            }
            if self.kbd.pending() {
                let c = self.kbd.read()?;
                self.key(c);
                continue;
            }
            let mut fds = vec![Ready::Read(self.mouse.fd()), Ready::Read(self.kbd.fd())];
            let wait_i = self.waitfd.as_ref().map(|f| {
                fds.push(Ready::Read(f.as_raw_fd()));
                fds.len() - 1
            });
            let base = fds.len();
            let ids: Vec<u32> = self.wins.iter().map(|w| w.id).collect();
            fds.extend(self.wins.iter().map(|w| Ready::Read(w.sess.io.as_raw_fd())));
            match purgatorio::ready(&fds, -1)? {
                Some(0) => {
                    let m = self.mouse.read()?;
                    self.mouse_event(m)?;
                }
                Some(1) => {
                    let c = self.kbd.read()?;
                    self.key(c);
                }
                Some(i) if Some(i) == wait_i => self.waited()?,
                Some(i) if i >= base => {
                    let id = ids[i - base];
                    if !self.input(id)? {
                        self.delete(id)?;
                    }
                }
                _ => {}
            }
        }
    }
}

/// `rio -W [cmd]`: the window's side. Mounts the window's files on fd 0
/// before `/dev`, then runs the command on the window's `cons` and waits
/// for it.
fn window_side(cmd: Option<&String>) -> Result<(), Error> {
    progctl(purgatorio::pid(), "catchint on");
    purgatorio::mount(0, -1, "/dev", mount::MBEFORE | mount::MNOAUTH, "")
        .map_err(|e| Error(format!("rio: cannot mount window: {e}")))?;
    let cons = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/cons")?;
    let fd = cons.as_raw_fd();
    let argv: Vec<String> = match cmd {
        Some(c) => vec![String::from("sh"), String::from("-c"), c.clone()],
        None => vec![String::from("sh"), String::from("-i")],
    };
    purgatorio::spawn(
        "/bin/sh.wasm",
        &argv,
        pctl::NEWFD,
        &[(fd, 0), (fd, 1), (fd, 2)],
    )?;
    drop(cons);
    // An interrupt typed in the window is for the shell, not for this.
    loop {
        match purgatorio::wait() {
            Ok(_) => return Ok(()),
            Err(e) if e.0.contains("interrupt") => continue,
            Err(_) => return Ok(()),
        }
    }
}

fn usage() -> Error {
    Error::from("usage: rio [-f font] [-i cmd]")
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("-W") {
        return window_side(args.get(2));
    }
    let mut font = None;
    let mut init = None;
    let mut a = args.iter().skip(1);
    while let Some(o) = a.next() {
        match o.as_str() {
            "-f" => font = Some(a.next().ok_or_else(usage)?.clone()),
            "-i" => init = Some(a.next().ok_or_else(usage)?.clone()),
            _ => return Err(usage()),
        }
    }
    let font = match font {
        Some(f) => Font::open(&f)?,
        None => Font::default_font()?,
    };
    let mut rio = Rio::new(font)?;
    if let Some(cmd) = init {
        let r = rio.default_rect();
        rio.new_window(r, &cmd)?;
    }
    rio.run()
}
