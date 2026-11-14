//! The commands and mouse actions that came after acme's first slice:
//! `Edit` and the `<`, `|` and `>` commands on the selection, `Zerox`,
//! `Kill`, `Sort`, `Tab`, `Indent`, `Font`, the 2-1 chord's argument,
//! moving and resizing windows and columns with the boxes, autoscroll
//! while sweeping, keeping the views of a shared text in step, and the
//! snarf buffer on `/dev/snarf`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::pctl;
use purgatorio::draw::{Point, Pointer};
use purgatorio::{Error, Ready};

use crate::edit::{self, FileInfo};
use crate::ui::{BORDER, Fnt, Kind, Text, Window};
use crate::{Acme, Tid};

/// `acme --feed`: copies its standard input to its standard output once
/// all of it has come, so acme can hand a command its input without
/// waiting on the command.
pub const FEED: &str = "--feed";

/// The fixed-width font `Font` switches to when `-F` names none.
pub const FIXED_FONT: &str = "/fonts/mono/unicode.13.font";

/// How often a sweep past a text's edge scrolls it.
const AUTOSCROLL_MS: i32 = 80;

/// The snarf buffer shared with other programs, when it exists.
const SNARF: &str = "/dev/snarf";

/// The body of `acme --feed`.
pub fn feed() -> Result<(), Error> {
    let mut data = Vec::new();
    std::io::stdin().read_to_end(&mut data)?;
    // The command may stop reading; that is not an error here.
    let _ = std::io::stdout().write_all(&data);
    Ok(())
}

impl Acme {
    // ---- Views of shared texts.

    /// Moves the selection and first rune of each view of a shared text
    /// past the edits made through the other views, and rewrites their
    /// tags.
    pub(crate) fn sync_views(&mut self) {
        // A view's tag says whether the text is modified, which a Put
        // through another view changes.
        let mut retag: Vec<u64> = self
            .cols
            .iter()
            .flat_map(|c| c.wins.iter())
            .filter(|w| w.body.b.views() > 1)
            .map(|w| w.id)
            .collect();
        let n = self.cols.len();
        for ci in 0..n {
            for wi in 0..self.cols[ci].wins.len() {
                let b = &self.cols[ci].wins[wi].body.b;
                let ops = b.take_ops();
                if ops.is_empty() {
                    continue;
                }
                let key = b.key();
                for c in &mut self.cols {
                    for w in &mut c.wins {
                        if w.body.b.key() != key {
                            continue;
                        }
                        let v = w.body.b.view();
                        let t = &mut w.body;
                        for op in ops.iter().filter(|o| o.view != v) {
                            t.q0 = op.fix(t.q0);
                            t.q1 = op.fix(t.q1);
                            t.org = op.fix(t.org);
                        }
                        t.fix_org();
                        let len = t.b.len();
                        t.q1 = t.q1.min(len);
                        t.q0 = t.q0.min(t.q1);
                        t.dirty = true;
                        retag.push(w.id);
                    }
                }
            }
        }
        for id in retag {
            self.retag(id);
        }
    }

    // ---- The mouse.

    /// Like [`Acme::track`], but while the pointer is above or below
    /// body `t` the body scrolls a line at a time and `f` sees the
    /// pointer again, so a sweep goes on past the frame's edge.
    pub(crate) fn track_auto(
        &mut self,
        t: Tid,
        mut f: impl FnMut(&mut Acme, Pointer) -> Result<(), Error>,
    ) -> Result<(), Error> {
        while self.m.buttons & 7 != 0 {
            self.redraw()?;
            let dir = match (t, self.text(t)) {
                (Tid::Body(_), Some(x)) if self.m.xy.y < x.r.min.y => -1,
                (Tid::Body(_), Some(x)) if self.m.xy.y >= x.r.max.y => 1,
                _ => 0,
            };
            let m = if dir == 0 {
                self.mouse.read()?
            } else {
                match purgatorio::ready(&[Ready::Read(self.mouse.fd())], AUTOSCROLL_MS)? {
                    Some(_) => self.mouse.read()?,
                    None => {
                        if let Some(id) = Acme::win_of(t) {
                            self.scroll(id, dir);
                            self.redraw()?;
                        }
                        self.m
                    }
                }
            };
            f(self, m)?;
            self.m = m;
        }
        Ok(())
    }

    /// The argument the 2-1 chord gives a command: the text last
    /// selected with button 1.
    pub(crate) fn argtext(&self) -> String {
        self.argsel
            .and_then(|s| self.text(s))
            .map(Text::selection)
            .unwrap_or_default()
    }

    /// A button on a window's box. Button 1 dragged moves the window to
    /// where it is let go, in whichever column that is; clicked, it
    /// makes the window bigger. Button 2 makes it as big as the column
    /// allows, leaving the other windows their tags, as button 3 does.
    pub(crate) fn drag_win(&mut self, id: u64, m: Pointer, pressed: u32) -> Result<(), Error> {
        let start = m.xy;
        let mut last = m.xy;
        self.track(|_, n| {
            last = n.xy;
            Ok(())
        })?;
        let moved = (last.x - start.x).abs() > 3 || (last.y - start.y).abs() > 3;
        if pressed & 1 != 0 && moved {
            self.move_win(id, last);
        } else if pressed & 1 != 0 {
            self.grow_win(id, false);
        } else {
            self.grow_win(id, true);
        }
        Ok(())
    }

    /// The smallest a window can be: its tag and the border.
    fn min_win(&self) -> i32 {
        self.fh() + 1 + BORDER
    }

    /// Moves window `id` so its top is at `p`.
    pub(crate) fn move_win(&mut self, id: u64, p: Point) {
        let Some((ci, wi)) = self.find(id) else {
            return;
        };
        let fh = self.fh();
        let to = self
            .cols
            .iter()
            .position(|c| p.x >= c.r.min.x && p.x < c.r.max.x)
            .unwrap_or(self.cols.len() - 1);
        let w = self.cols[ci].wins.remove(wi);
        // The window above takes the space it leaves.
        if wi == 0
            && let Some(first) = self.cols[ci].wins.first_mut()
        {
            first.y = w.y;
        }
        let min = self.min_win();
        let c = &mut self.cols[to];
        let top = c.top(fh);
        let y = p.y.clamp(top, (c.r.max.y - min).max(top));
        let i = c.wins.iter().take_while(|o| o.y < y).count();
        let mut w = w;
        w.y = y;
        if i == 0 {
            w.y = top;
            if let Some(first) = c.wins.first_mut() {
                first.y = first.y.max(y.max(top + min));
            }
        }
        c.wins.insert(i, w);
        for k in [ci, to] {
            self.cols[k].place(fh, &mut self.f);
        }
        self.full = true;
    }

    /// Makes window `id` bigger: to the most the column allows when
    /// `max`, else to double its height or half the column.
    pub(crate) fn grow_win(&mut self, id: u64, max: bool) {
        let Some((ci, wi)) = self.find(id) else {
            return;
        };
        let fh = self.fh();
        let min = self.min_win();
        let c = &mut self.cols[ci];
        let top = c.top(fh);
        let total = c.r.max.y - top + BORDER;
        let n = c.wins.len() as i32;
        let mut h: Vec<i32> = c.wins.iter().map(|w| w.bottom - w.y + BORDER).collect();
        let most = total - (n - 1) * min;
        let want = if max {
            most
        } else {
            (2 * h[wi]).max(total / 2).min(most)
        };
        let rest_old: i32 = h
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != wi)
            .map(|(_, v)| *v)
            .sum();
        let rest_new = total - want;
        for (i, v) in h.iter_mut().enumerate() {
            if i == wi {
                *v = want;
            } else if rest_old > 0 {
                *v = (*v * rest_new / rest_old).max(min);
            } else {
                *v = min;
            }
        }
        let mut y = top;
        for (w, v) in c.wins.iter_mut().zip(&h) {
            w.y = y;
            y += v;
        }
        c.place(fh, &mut self.f);
        self.full = true;
    }

    /// Button 1 dragged on a column's box moves the column's left edge.
    pub(crate) fn drag_col(&mut self, ci: usize, m: Pointer) -> Result<(), Error> {
        let start = m.xy;
        let mut last = m.xy;
        self.track(|_, n| {
            last = n.xy;
            Ok(())
        })?;
        if m.buttons & 1 == 0 || ci == 0 || (last.x - start.x).abs() <= 3 {
            return Ok(());
        }
        let minw = 3 * crate::ui::SCROLLWID + 2 * BORDER;
        let lo = self.cols[ci - 1].r.min.x + minw;
        let hi = self.cols[ci].r.max.x - minw;
        if lo > hi {
            return Ok(());
        }
        self.cols[ci].r.min.x = last.x.clamp(lo, hi);
        self.place();
        Ok(())
    }

    // ---- Snarf.

    /// Sets the snarf buffer: `/dev/snarf` when it is there, so other
    /// programs share it, and acme's own copy.
    pub(crate) fn set_snarf(&mut self, s: String) {
        if let Ok(mut f) = OpenOptions::new().write(true).open(SNARF) {
            let _ = f.write_all(s.as_bytes());
        }
        self.snarf = s;
    }

    /// The snarf buffer: `/dev/snarf` when it can be read, else acme's
    /// own.
    pub(crate) fn get_snarf(&self) -> String {
        match std::fs::read(SNARF) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => self.snarf.clone(),
        }
    }

    // ---- Edit.

    /// Runs `Edit` command line `s` for text `t`: on the window it is in,
    /// or from a row or column tag the one last selected in.
    pub(crate) fn edit(&mut self, t: Tid, s: &str) {
        let cur = self.target_win(t);
        let dir = self.dir_of(t);
        let ci = self.col_of(t);
        let mut h = EditHost {
            a: self,
            dir,
            ci,
            touched: Vec::new(),
            marked: Vec::new(),
            win: Acme::win_of(t),
        };
        let _ = edit::run(&mut h, cur, s);
        let touched = h.touched;
        for id in touched {
            if let Some(w) = self.win(id) {
                let q = w.body.q0;
                self.show(Tid::Body(id), q);
                let _ = self.edited(Tid::Body(id));
            }
        }
    }

    /// Runs command line `cmd` with sh in `dir`, with `input` on its
    /// standard input, and returns its output when it has finished.
    /// Meanwhile acme goes on: it serves `/mnt/acme`, draws, and takes
    /// the mouse and keyboard, so the command may use acme's files and
    /// `Kill` can stop it. The texts with keys in `locked` are the ones
    /// the `Edit` holds positions in: until the command ends nothing
    /// but the `Edit` changes them (see [`Acme::locked`]).
    pub(crate) fn run_edit(
        &mut self,
        dir: &str,
        cmd: &str,
        input: Option<&str>,
        win: Option<u64>,
        locked: Vec<usize>,
    ) -> String {
        let (pid, mut r) = match spawn_edit(dir, cmd, input, win) {
            Ok(x) => x,
            Err(e) => return format!("{cmd}: {e}\n"),
        };
        self.edit_run = Some(EditRun {
            pid,
            win,
            name: cmd.split_whitespace().next().unwrap_or("").to_string(),
            locked,
        });
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match self.step(Some(r.as_raw_fd())) {
                Ok(true) => match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                },
                Ok(false) => {}
                Err(_) => {
                    // The screen or the keyboard went: just wait for the
                    // command; the main loop sees the error again.
                    let _ = r.read_to_end(&mut out);
                    break;
                }
            }
        }
        self.edit_run = None;
        for (dir, s) in std::mem::take(&mut self.held) {
            self.append(&dir, &s);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Whether `t` is a text an `Edit` holds positions in while its
    /// command runs. Such a text changes only through the `Edit`:
    /// typing, cut and paste, `Undo`, `Redo` and `Get` leave it alone,
    /// acme(4) writes that would change it fail, and output for it waits
    /// until the command ends.
    pub(crate) fn locked(&self, t: Tid) -> bool {
        let (Some(r), Tid::Body(id)) = (&self.edit_run, t) else {
            return false;
        };
        self.win(id)
            .is_some_and(|w| r.locked.contains(&w.body.b.key()))
    }

    /// Whether an `Edit` is waiting for a command.
    pub(crate) fn editing(&self) -> bool {
        self.edit_run.is_some()
    }

    // ---- Window commands.

    /// `Zerox`: another window on the same text.
    pub(crate) fn zerox(&mut self, t: Tid) {
        let Some(id) = self.target_win(t) else {
            return;
        };
        let Some((ci, _)) = self.find(id) else {
            return;
        };
        let nid = self.next_id;
        self.next_id += 1;
        let w = self.win(id).unwrap_or_else(|| unreachable!());
        let mut body = Text::new(Kind::Body);
        body.b = w.body.b.share();
        body.font = w.body.font;
        body.tab = w.body.tab;
        body.org = w.body.org;
        body.set_select(w.body.q0, w.body.q1);
        let mut nw = Window::new(nid, body);
        nw.is_dir = w.is_dir;
        nw.indent = w.indent;
        let name = w.name();
        self.add_win(ci, nw, &name);
    }

    /// `Kill [name ...]`: kills the commands with those names, or with
    /// none, those started from this window (from a row or column tag,
    /// all of them).
    pub(crate) fn kill(&mut self, t: Tid, arg: &str) {
        let win = Acme::win_of(t);
        let names: Vec<&str> = arg.split_whitespace().collect();
        // The commands started from tags, and the one an `Edit` waits
        // for.
        let running = self
            .children
            .iter()
            .map(|c| (c.pid, c.win, c.name.as_str()))
            .chain(
                self.edit_run
                    .iter()
                    .map(|r| (r.pid, r.win, r.name.as_str())),
            );
        for (pid, cwin, name) in running {
            let hit = if names.is_empty() {
                win.is_none() || cwin == win
            } else {
                names.contains(&name)
            };
            if hit
                && let Ok(mut f) = OpenOptions::new()
                    .write(true)
                    .open(format!("/prog/{pid}/ctl"))
            {
                let _ = f.write_all(b"killgrp");
            }
        }
    }

    /// `Sort`: puts the windows of the column in order of their names,
    /// each keeping its height.
    pub(crate) fn sort(&mut self, t: Tid) {
        let ci = self.col_of(t);
        let fh = self.fh();
        let Some(c) = self.cols.get_mut(ci) else {
            return;
        };
        let mut wins: Vec<(i32, Window)> = c
            .wins
            .drain(..)
            .map(|w| (w.bottom - w.y + BORDER, w))
            .collect();
        wins.sort_by_key(|(_, w)| w.name());
        let mut y = c.top(fh);
        for (h, mut w) in wins {
            w.y = y;
            y += h;
            c.wins.push(w);
        }
        c.place(fh, &mut self.f);
        self.full = true;
    }

    /// `Tab [n]`: sets the window's tab stops to every n zeros, or says
    /// what they are.
    pub(crate) fn tab(&mut self, t: Tid, arg: &str) {
        let Some(id) = self.target_win(t) else {
            return;
        };
        let cwd = self.cwd.clone();
        let Some(w) = self.win_mut(id) else {
            return;
        };
        match arg.parse::<usize>() {
            Ok(n) if n > 0 => {
                w.body.tab = Some(n);
                w.body.dirty = true;
            }
            _ => {
                let msg = format!("{}: Tab {}", w.name(), w.body.tab.unwrap_or(crate::ui::TAB));
                let dir = w.dir(&cwd);
                self.error(&dir, &msg);
            }
        }
    }

    /// `Indent [on|off]`: turns automatic indenting on or off for all
    /// windows, or alone, for this window the other way.
    pub(crate) fn indent(&mut self, t: Tid, arg: &str) {
        match arg.to_ascii_lowercase().as_str() {
            "on" | "off" => {
                let on = arg.eq_ignore_ascii_case("on");
                self.indent = on;
                for w in self.cols.iter_mut().flat_map(|c| c.wins.iter_mut()) {
                    w.indent = on;
                }
            }
            _ => {
                if let Some(id) = self.target_win(t)
                    && let Some(w) = self.win_mut(id)
                {
                    w.indent = !w.indent;
                }
            }
        }
    }

    /// The index of font `name` for a text, opening it if need be.
    fn font_index(&mut self, name: &str) -> Result<usize, Error> {
        if name == self.f.name {
            return Ok(0);
        }
        if let Some(k) = self.fonts.iter().position(|f| f.name == name) {
            return Ok(k + 1);
        }
        let f = purgatorio::draw::Font::open(name)?;
        self.fonts.push(Fnt::new(f, name));
        Ok(self.fonts.len())
    }

    /// The index of font `name`, or of the main font if it cannot be
    /// opened.
    pub(crate) fn font_index_or_main(&mut self, name: &str) -> usize {
        self.font_index(name).unwrap_or(0)
    }

    /// `Font [name]`: switches the window's body between the main and
    /// the fixed-width font, or to font `name`.
    pub(crate) fn font(&mut self, t: Tid, arg: &str) {
        let Some(id) = self.target_win(t) else {
            return;
        };
        let Some(cur) = self.win(id).map(|w| w.body.font) else {
            return;
        };
        let name = if !arg.is_empty() {
            arg.to_string()
        } else if cur == 0 {
            self.fix_name.clone()
        } else {
            self.f.name.clone()
        };
        let dir = self.dir_of(t);
        match self.font_index(&name) {
            Ok(k) => {
                if let Some(w) = self.win_mut(id) {
                    w.body.font = k;
                    w.body.dirty = true;
                }
                let q = self.win(id).map_or(0, |w| w.body.q0);
                self.show(Tid::Body(id), q);
            }
            Err(e) => self.error(&dir, &format!("{e}")),
        }
    }

    /// The name of the font text `x` is drawn in.
    pub(crate) fn font_name(&self, x: &Text) -> String {
        match x.font {
            0 => self.f.name.clone(),
            k => self
                .fonts
                .get(k - 1)
                .map_or_else(|| self.f.name.clone(), |f| f.name.clone()),
        }
    }
}

/// The command an `Edit` waits for while acme goes on.
pub struct EditRun {
    pid: u32,
    /// The window the `Edit` ran from, for `Kill`.
    win: Option<u64>,
    /// The command's name, for `Kill name`.
    name: String,
    /// The keys of the texts it holds.
    locked: Vec<usize>,
}

/// Starts `cmd` for [`Acme::run_edit`]: its pid and the pipe its
/// standard output and error come on.
fn spawn_edit(
    dir: &str,
    cmd: &str,
    input: Option<&str>,
    win: Option<u64>,
) -> Result<(u32, File), Error> {
    let [r, w] = purgatorio::pipe()?;
    // As for other commands, $winid names the window.
    let winid = win.map_or_else(String::new, |id| format!("winid={id}; "));
    let argv = vec![
        String::from("sh"),
        String::from("-c"),
        format!("{winid}cd {}; {}", crate::quote(dir), cmd),
    ];
    let mut feeder = None;
    let stdin = match input {
        None => purgatorio::open("/dev/null", purgatorio::abi::mode::OREAD)?,
        Some(_) => {
            // acme writes to the feeder, which takes it all before it
            // writes to the command: acme never waits on the command
            // while the command waits for acme to read its output.
            let [ir, iw] = purgatorio::pipe()?;
            let [cr, cw] = purgatorio::pipe()?;
            purgatorio::spawn(
                "/bin/acme.wasm",
                &[String::from("acme"), String::from(FEED)],
                pctl::NEWFD,
                &[(ir.as_raw_fd(), 0), (cw.as_raw_fd(), 1), (2, 2)],
            )?;
            feeder = Some(File::from(iw));
            cr
        }
    };
    let flags = pctl::NEWFD | pctl::FORKNS | pctl::NEWPGRP;
    let map = [
        (stdin.as_raw_fd(), 0),
        (w.as_raw_fd(), 1),
        (w.as_raw_fd(), 2),
    ];
    let spawned = purgatorio::spawn("sh", &argv, flags, &map);
    drop(stdin);
    drop(w);
    if let (Some(mut f), Some(s)) = (feeder, input) {
        let _ = f.write_all(s.as_bytes());
    }
    Ok((spawned?, File::from(r)))
}

/// acme as `Edit` sees it.
struct EditHost<'a> {
    a: &'a mut Acme,
    /// The directory of the text `Edit` ran from, where its output goes.
    dir: String,
    /// The column new windows go in.
    ci: usize,
    /// Windows whose text or dot changed, to show and retag after.
    touched: Vec<u64>,
    /// Windows whose undo log has a new sequence for this `Edit`.
    marked: Vec<u64>,
    /// The window the `Edit` ran from, for `Kill`.
    win: Option<u64>,
}

impl EditHost<'_> {
    fn touch(&mut self, id: u64) {
        if !self.touched.contains(&id) {
            self.touched.push(id);
        }
    }
}

impl edit::Host for EditHost<'_> {
    fn files(&self) -> Vec<FileInfo> {
        let mut keys = Vec::new();
        let mut out = Vec::new();
        for id in self.a.ids() {
            let Some(w) = self.a.win(id) else {
                continue;
            };
            let k = w.body.b.key();
            if keys.contains(&k) {
                continue;
            }
            keys.push(k);
            out.push(FileInfo {
                id,
                name: w.name(),
                dirty: w.body.b.dirty(),
            });
        }
        out
    }

    fn text(&self, id: u64) -> Vec<char> {
        self.a
            .win(id)
            .map(|w| w.body.b.all().chars().collect())
            .unwrap_or_default()
    }

    fn dot(&self, id: u64) -> (usize, usize) {
        self.a.win(id).map_or((0, 0), |w| (w.body.q0, w.body.q1))
    }

    fn set_dot(&mut self, id: u64, r: (usize, usize)) {
        if let Some(w) = self.a.win_mut(id) {
            w.body.set_select(r.0, r.1);
        }
        self.touch(id);
    }

    fn replace(&mut self, id: u64, q0: usize, q1: usize, s: &str) {
        let first = !self.marked.contains(&id);
        if let Some(w) = self.a.win_mut(id) {
            if first {
                w.body.b.mark();
            }
            w.body.edit(q0, q1, s);
        }
        if first {
            self.marked.push(id);
        }
        self.touch(id);
    }

    fn undo(&mut self, id: u64, redo: bool) -> bool {
        self.touch(id);
        let Some(w) = self.a.win_mut(id) else {
            return false;
        };
        let r = if redo {
            w.body.b.redo()
        } else {
            w.body.b.undo()
        };
        match r {
            Some((q0, q1)) => {
                w.body.set_select(q0, q1);
                w.body.fix_org();
                // Later changes in this Edit are a new step to undo.
                self.marked.retain(|m| *m != id);
                true
            }
            None => false,
        }
    }

    fn set_name(&mut self, id: u64, name: &str) {
        if let Some(w) = self.a.win_mut(id) {
            crate::set_tag(w, name);
        }
    }

    fn set_clean(&mut self, id: u64) {
        if let Some(w) = self.a.win_mut(id) {
            w.body.b.set_clean();
            w.warned = false;
        }
        self.touch(id);
    }

    fn dir(&self, id: Option<u64>) -> String {
        match id.and_then(|id| self.a.win(id)) {
            Some(w) => w.dir(&self.a.cwd),
            None => self.dir.clone(),
        }
    }

    fn open(&mut self, path: &str) -> Result<u64, String> {
        if crate::own(path) {
            return Err(format!("{path}: {}", crate::EOWN));
        }
        let path = crate::clean(path);
        Ok(self.a.open(self.ci, &path, None))
    }

    fn close(&mut self, id: u64) -> Result<(), String> {
        self.touched.retain(|t| *t != id);
        self.a.del_win(id);
        Ok(())
    }

    fn read_file(&mut self, path: &str) -> Result<String, String> {
        Acme::read(path)
            .map(|(s, _)| s)
            .map_err(|e| format!("{path}: {e}"))
    }

    fn write_file(&mut self, path: &str, s: &str) -> Result<(), String> {
        if crate::own(path) {
            return Err(format!("{path}: {}", crate::EOWN));
        }
        std::fs::write(path, s.as_bytes()).map_err(|_| format!("{path}: {}", purgatorio::errstr()))
    }

    fn run(&mut self, dir: &str, cmd: &str, input: Option<&str>, busy: &[u64]) -> String {
        let locked = busy
            .iter()
            .filter_map(|id| self.a.win(*id))
            .map(|w| w.body.b.key())
            .collect();
        let win = self.win;
        self.a.run_edit(dir, cmd, input, win, locked)
    }

    fn print(&mut self, s: &str) {
        let dir = self.dir.clone();
        self.a.append(&dir, s);
    }
}
