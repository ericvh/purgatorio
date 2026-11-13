//! draw/faces: a face for each command running, after Plan 9's and
//! Inferno's `faces`, which show a face for each sender of new mail.
//! Purgatorio has no mail; what comes and goes here is progs, so the
//! faces are the commands in `/prog`: one tile per command name, with a
//! face made from a hash of the name (head colour and shape, eyes, mouth,
//! hair or hat), the name under it and, when more than one prog runs it,
//! a count. A command that starts gets a highlighted tile for a few
//! seconds, as new mail does; one that ends loses its tile.
//!
//! `draw/faces [-i seconds]` reads `/prog` every `seconds` (default 2).
//! Button 1 on a face shows its progs (pid, user, state) on the bottom
//! line; button 1 elsewhere clears it. `q` or Delete ends the program.
//! It follows a resize by checking the screen's size on each pointer
//! event and each scan.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use purgatorio::arg::Arg;
use purgatorio::draw::{Display, ENDDISC, Font, Image, Keyboard, Mouse, Point, Pointer, Rect};
use purgatorio::{Context, Error, Ready};

/// A face's square, in pixels.
const FACE: i32 = 56;
/// The space between tiles and round the edge.
const GAP: i32 = 10;
/// How long a new command's tile stays highlighted.
const NEW: Duration = Duration::from_secs(5);

/// One prog, from `/prog/<pid>/status`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Prog {
    pid: u32,
    user: String,
    state: String,
    name: String,
}

/// Parses `/prog/<pid>/status`: pid, parent, group, user, state, name.
fn parse_status(s: &str) -> Option<Prog> {
    let f: Vec<&str> = s.split_whitespace().collect();
    if f.len() < 6 {
        return None;
    }
    Some(Prog {
        pid: f[0].parse().ok()?,
        user: f[3].to_string(),
        state: f[4].to_string(),
        name: f[5..].join(" "),
    })
}

fn scan() -> Vec<Prog> {
    let Ok(dir) = std::fs::read_dir("/prog") else {
        return Vec::new();
    };
    let mut out: Vec<Prog> = dir
        .filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse::<u32>().ok())
        .filter_map(|pid| {
            // A prog may exit between the listing and the read.
            let s = std::fs::read_to_string(format!("/prog/{pid}/status")).ok()?;
            parse_status(&s)
        })
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

/// The progs grouped by command name, in name order.
fn group(progs: &[Prog]) -> Vec<(String, Vec<Prog>)> {
    let mut g: Vec<(String, Vec<Prog>)> = Vec::new();
    for p in progs {
        match g.iter_mut().find(|(n, _)| *n == p.name) {
            Some((_, v)) => v.push(p.clone()),
            None => g.push((p.name.clone(), vec![p.clone()])),
        }
    }
    g.sort_by(|a, b| a.0.cmp(&b.0));
    g
}

/// FNV-1a with murmur3's final mix, so a name always gets the same face
/// and similar names get different ones.
fn hash(s: &str) -> u32 {
    let mut h = s.bytes().fold(0x811C_9DC5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    h ^= h >> 16;
    h = h.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

/// `0xRRGGBBFF` for hue `h` (degrees), saturation and value in 0..=1.
fn hsv(h: f64, s: f64, v: f64) -> u32 {
    let c = v * s;
    let hp = (h % 360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let byte = |f: f64| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u32;
    byte(r) << 24 | byte(g) << 16 | byte(b) << 8 | 0xFF
}

/// What a face looks like, from its name's hash.
#[derive(Debug, PartialEq, Eq)]
struct Look {
    /// Hue in 15-degree steps, 0..24.
    hue: u32,
    /// Head height less its width, -2..=2.
    tall: i32,
    /// 0 bare, 1 hair, 2 hat, 3 antenna.
    top: u32,
    /// 0 dots, 1 round with pupils, 2 sleepy, 3 glasses.
    eyes: u32,
    /// 0 smile, 1 grin, 2 surprised, 3 straight.
    mouth: u32,
    cheeks: bool,
    /// Which way round eyes look: -1, 0 or 1.
    glance: i32,
}

fn look(name: &str) -> Look {
    let h = hash(name);
    Look {
        hue: h % 24,
        tall: ((h >> 5) % 5) as i32 - 2,
        top: (h >> 8) & 3,
        eyes: (h >> 10) & 3,
        mouth: (h >> 12) & 3,
        cheeks: (h >> 14) & 1 == 1,
        glance: ((h >> 15) % 3) as i32 - 1,
    }
}

struct Faces {
    d: Display,
    font: Font,
    colours: HashMap<u32, Image>,
    groups: Vec<(String, Vec<Prog>)>,
    /// When each command was first seen, if after the first scan.
    born: HashMap<String, Instant>,
    selected: Option<String>,
}

impl Faces {
    fn colour(&mut self, rgba: u32) -> Result<Image, Error> {
        if let Some(i) = self.colours.get(&rgba) {
            return Ok(i.clone());
        }
        let i = self.d.color(rgba)?;
        self.colours.insert(rgba, i.clone());
        Ok(i)
    }

    /// The tiles' size and the number across.
    fn grid(&self) -> (i32, i32, i32) {
        let r = self.d.screen().r;
        let tw = FACE + 20;
        let th = FACE + 6 + self.font.height;
        let across = ((r.dx() - GAP) / (tw + GAP)).max(1);
        (tw, th, across)
    }

    fn tile(&self, i: usize) -> Rect {
        let r = self.d.screen().r;
        let (tw, th, across) = self.grid();
        let (col, row) = (i as i32 % across, i as i32 / across);
        let x = r.min.x + GAP + col * (tw + GAP);
        let y = r.min.y + GAP + self.font.height + 6 + row * (th + GAP);
        Rect::new(x, y, x + tw, y + th)
    }

    fn hit(&self, p: Point) -> Option<usize> {
        (0..self.groups.len()).find(|&i| self.tile(i).contains(p))
    }

    /// Reads `/prog` again; true when the faces changed.
    fn update(&mut self, first: bool) -> bool {
        let g = group(&scan());
        let now = Instant::now();
        for (name, _) in &g {
            if !first && !self.groups.iter().any(|(n, _)| n == name) {
                self.born.insert(name.clone(), now);
            }
        }
        self.born.retain(|n, _| g.iter().any(|(m, _)| m == n));
        if self
            .selected
            .as_ref()
            .is_some_and(|s| !g.iter().any(|(n, _)| n == s))
        {
            self.selected = None;
        }
        let changed = g != self.groups;
        self.groups = g;
        changed
    }

    /// When the next highlight ends, if one is showing.
    fn next_fade(&self) -> Option<Instant> {
        self.born.values().map(|&b| b + NEW).min()
    }

    fn redraw(&mut self) -> Result<(), Error> {
        let now = Instant::now();
        self.born.retain(|_, b| now < *b + NEW);
        let screen = self.d.screen();
        let r = screen.r;
        let zero = Point::new(0, 0);
        let back = self.colour(0xF2F0E6FF)?;
        let ink = self.colour(0x202020FF)?;
        let grey = self.colour(0x8C8A80FF)?;
        self.d.draw(&screen, r, &back, None, zero)?;
        let nprogs: usize = self.groups.iter().map(|(_, v)| v.len()).sum();
        let title = format!(
            "{} prog{}, {} command{}",
            nprogs,
            if nprogs == 1 { "" } else { "s" },
            self.groups.len(),
            if self.groups.len() == 1 { "" } else { "s" }
        );
        let at = Point::new(r.min.x + GAP, r.min.y + 4);
        self.d
            .string(&screen, at, &grey, zero, &mut self.font, &title)?;
        for i in 0..self.groups.len() {
            let t = self.tile(i);
            if t.min.y >= r.max.y - self.font.height - 4 {
                break;
            }
            self.draw_tile(&screen, i, t)?;
        }
        let status = self.selected.as_ref().and_then(|s| {
            let (name, progs) = self.groups.iter().find(|(n, _)| n == s)?;
            let list: Vec<String> = progs
                .iter()
                .map(|p| format!("{} {} {}", p.pid, p.user, p.state))
                .collect();
            Some(format!("{name}: {}", list.join(", ")))
        });
        if let Some(s) = status {
            let y = r.max.y - self.font.height - 4;
            let line = Rect::new(r.min.x, y - 2, r.max.x, r.max.y);
            let bar = self.colour(0xE2DFD0FF)?;
            self.d.draw(&screen, line, &bar, None, zero)?;
            let at = Point::new(r.min.x + GAP, y);
            self.d.string(&screen, at, &ink, zero, &mut self.font, &s)?;
        }
        self.d.flush()
    }

    fn draw_tile(&mut self, dst: &Image, i: usize, t: Rect) -> Result<(), Error> {
        let zero = Point::new(0, 0);
        let (name, progs) = self.groups[i].clone();
        let new = self.born.contains_key(&name);
        let chosen = self.selected.as_ref() == Some(&name);
        let card = self.colour(if new { 0xFFF3B0FF } else { 0xFFFFFFFF })?;
        let edge = self.colour(if chosen {
            0x2C5BD8FF
        } else if new {
            0xE0A800FF
        } else {
            0xC8C4B4FF
        })?;
        self.d.draw(dst, t, &card, None, zero)?;
        let thick = if chosen || new { 2 } else { 1 };
        for side in [
            Rect::new(t.min.x, t.min.y, t.max.x, t.min.y + thick),
            Rect::new(t.min.x, t.max.y - thick, t.max.x, t.max.y),
            Rect::new(t.min.x, t.min.y, t.min.x + thick, t.max.y),
            Rect::new(t.max.x - thick, t.min.y, t.max.x, t.max.y),
        ] {
            self.d.draw(dst, side, &edge, None, zero)?;
        }
        let c = Point::new(t.min.x + t.dx() / 2, t.min.y + 4 + FACE / 2);
        self.face(dst, c, &look(&name))?;
        // The name, shortened to fit.
        let ink = self.colour(0x202020FF)?;
        let short = name.clone();
        let mut label = short.clone();
        while self.font.string_width(&label) > t.dx() - 6 && label.chars().count() > 1 {
            let n = label.chars().count();
            label = short.chars().take(n.saturating_sub(3)).collect::<String>() + "..";
            if n <= 3 {
                break;
            }
        }
        let w = self.font.string_width(&label);
        let at = Point::new(t.min.x + (t.dx() - w) / 2, t.max.y - self.font.height - 3);
        self.d.string(dst, at, &ink, zero, &mut self.font, &label)?;
        if progs.len() > 1 {
            let n = progs.len().to_string();
            let badge = self.colour(0xC0392BFF)?;
            let white = self.colour(0xFFFFFFFF)?;
            let br = (self.font.height / 2 + 1).max(8);
            let bc = Point::new(t.max.x - br - 2, t.min.y + br + 2);
            self.d.fill_ellipse(dst, bc, br, br, &badge, zero)?;
            let w = self.font.string_width(&n);
            let at = Point::new(bc.x - w / 2, bc.y - self.font.height / 2);
            self.d.string(dst, at, &white, zero, &mut self.font, &n)?;
        }
        Ok(())
    }

    /// Draws a face centred on `c`.
    fn face(&mut self, dst: &Image, c: Point, l: &Look) -> Result<(), Error> {
        let zero = Point::new(0, 0);
        let hue = f64::from(l.hue) * 15.0;
        let skin = self.colour(hsv(hue, 0.35, 0.98))?;
        let outline = self.colour(hsv(hue, 0.55, 0.45))?;
        let ink = self.colour(0x26221EFF)?;
        let white = self.colour(0xFFFFFFFF)?;
        let (a, b) = (20, 21 + l.tall);
        let head = Point::new(c.x, c.y + 3);
        // Hair goes behind the head; a hat and an antenna go on top.
        if l.top == 1 {
            let hair = self.colour(hsv((hue + 180.0) % 360.0, 0.5, 0.35))?;
            self.d.fill_ellipse(
                dst,
                Point::new(head.x, head.y - 3),
                a + 3,
                b + 2,
                &hair,
                zero,
            )?;
        }
        self.d.fill_ellipse(dst, head, a, b, &skin, zero)?;
        self.d.ellipse(dst, head, a, b, 0, &outline, zero)?;
        if l.top == 1 {
            let hair = self.colour(hsv((hue + 180.0) % 360.0, 0.5, 0.35))?;
            self.d.fill_arc(dst, head, a, b, &hair, zero, 20, 140)?;
        }
        let top = head.y - b;
        match l.top {
            2 => {
                let hat = self.colour(hsv((hue + 120.0) % 360.0, 0.6, 0.55))?;
                let brim = Rect::new(head.x - a - 3, top + 4, head.x + a + 3, top + 8);
                let crown = Rect::new(head.x - a + 6, top - 9, head.x + a - 6, top + 5);
                self.d.draw(dst, crown, &hat, None, zero)?;
                self.d.draw(dst, brim, &hat, None, zero)?;
            }
            3 => {
                let tip = Point::new(head.x + 6, (top - 9).max(c.y - FACE / 2));
                self.d.line(
                    dst,
                    Point::new(head.x + 2, top + 1),
                    tip,
                    ENDDISC,
                    ENDDISC,
                    0,
                    &ink,
                    zero,
                )?;
                let bulb = self.colour(hsv((hue + 60.0) % 360.0, 0.8, 0.95))?;
                self.d.fill_ellipse(dst, tip, 3, 3, &bulb, zero)?;
            }
            _ => {}
        }
        let ey = head.y - b / 4;
        for ex in [head.x - 8, head.x + 8] {
            let e = Point::new(ex, ey);
            match l.eyes {
                0 => self.d.fill_ellipse(dst, e, 2, 3, &ink, zero)?,
                1 => {
                    self.d.fill_ellipse(dst, e, 5, 5, &white, zero)?;
                    self.d.ellipse(dst, e, 5, 5, 0, &outline, zero)?;
                    let p = Point::new(ex + 2 * l.glance, ey + 1);
                    self.d.fill_ellipse(dst, p, 2, 2, &ink, zero)?;
                }
                2 => self.d.line(
                    dst,
                    Point::new(ex - 4, ey + 1),
                    Point::new(ex + 4, ey + 1),
                    ENDDISC,
                    ENDDISC,
                    0,
                    &ink,
                    zero,
                )?,
                _ => {
                    self.d.ellipse(dst, e, 6, 5, 1, &ink, zero)?;
                    self.d
                        .fill_ellipse(dst, Point::new(ex + l.glance, ey), 2, 2, &ink, zero)?;
                }
            }
        }
        if l.eyes == 3 {
            self.d.line(
                dst,
                Point::new(head.x - 2, ey),
                Point::new(head.x + 2, ey),
                ENDDISC,
                ENDDISC,
                0,
                &ink,
                zero,
            )?;
        }
        if l.cheeks {
            let pink = self.colour(0xF29A9AFF)?;
            for x in [head.x - 13, head.x + 13] {
                self.d
                    .fill_ellipse(dst, Point::new(x, head.y + 5), 3, 2, &pink, zero)?;
            }
        }
        let my = head.y + b / 2;
        let m = Point::new(head.x, my - 3);
        match l.mouth {
            0 => self.d.arc(dst, m, 8, 6, 1, &ink, zero, 200, 140)?,
            1 => {
                self.d.fill_arc(dst, m, 9, 7, &ink, zero, 180, 180)?;
                let teeth = Rect::new(m.x - 7, m.y, m.x + 7, m.y + 2);
                self.d.draw(dst, teeth, &white, None, zero)?;
            }
            2 => self
                .d
                .ellipse(dst, Point::new(head.x, my), 3, 4, 1, &ink, zero)?,
            _ => self.d.line(
                dst,
                Point::new(head.x - 6, my),
                Point::new(head.x + 6, my - 1),
                ENDDISC,
                ENDDISC,
                1,
                &ink,
                zero,
            )?,
        }
        Ok(())
    }
}

enum Event {
    Tick,
    Pointer(Pointer),
    Key(char),
}

fn next(mouse: &mut Mouse, kbd: &mut Option<Keyboard>, ms: i32) -> Result<Event, Error> {
    if let Some(k) = kbd
        && k.pending()
    {
        return Ok(Event::Key(k.read()?));
    }
    let mut fds = vec![Ready::Read(mouse.fd())];
    if let Some(k) = kbd {
        fds.push(Ready::Read(k.fd()));
    }
    match purgatorio::ready(&fds, ms)? {
        Some(0) => Ok(Event::Pointer(mouse.read()?)),
        Some(_) => match kbd {
            Some(k) => Ok(Event::Key(k.read()?)),
            None => Ok(Event::Tick),
        },
        None => Ok(Event::Tick),
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "draw/faces [-i seconds]");
    let mut every = Duration::from_secs(2);
    while let Some(c) = arg.opt() {
        match c {
            'i' => {
                let v = arg.earg()?;
                let s: f64 = v.parse().map_err(|_| arg.usage())?;
                if !(0.1..=3600.0).contains(&s) {
                    return Err(arg.usage());
                }
                every = Duration::from_secs_f64(s);
            }
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error::from("usage: draw/faces [-i seconds]"));
    }
    let mut f = Faces {
        d: Display::open()?,
        font: Font::open("/fonts/sans/unicode.13.font")?,
        colours: HashMap::new(),
        groups: Vec::new(),
        born: HashMap::new(),
        selected: None,
    };
    let mut mouse = Mouse::open()?;
    let mut kbd = Keyboard::open().ok();
    f.update(true);
    f.redraw()?;
    let mut scan_at = Instant::now() + every;
    let mut buttons = 0;
    loop {
        let now = Instant::now();
        let due = f.next_fade().map_or(scan_at, |t| t.min(scan_at));
        let ms = due
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, 60_000) as i32;
        let mut dirty = false;
        match next(&mut mouse, &mut kbd, ms)? {
            Event::Key('q' | '\x7F') => return Ok(()),
            Event::Key(_) => {}
            Event::Pointer(m) => {
                dirty |= f.d.check_resize()?;
                let pressed = m.buttons & !buttons;
                buttons = m.buttons;
                if pressed & 1 != 0 {
                    let sel = f.hit(m.xy).map(|i| f.groups[i].0.clone());
                    dirty |= sel != f.selected;
                    f.selected = sel;
                }
            }
            Event::Tick => {}
        }
        let now = Instant::now();
        if now >= scan_at {
            dirty |= f.d.check_resize()?;
            dirty |= f.update(false);
            scan_at = now + every;
        }
        if f.next_fade().is_some_and(|t| t <= now) {
            dirty = true;
        }
        if dirty {
            f.redraw()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status() {
        let p = parse_status("12 1 12 inferno running draw-faces\n").unwrap();
        assert_eq!(
            (p.pid, p.user.as_str(), p.name.as_str()),
            (12, "inferno", "draw-faces")
        );
        assert!(parse_status("12 1").is_none());
    }

    #[test]
    fn groups_by_name() {
        let p = |pid, name: &str| Prog {
            pid,
            user: String::from("u"),
            state: String::from("running"),
            name: name.to_string(),
        };
        let g = group(&[p(1, "sh"), p(2, "sleep"), p(3, "sh")]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, "sh");
        assert_eq!(g[0].1.len(), 2);
    }

    #[test]
    fn faces_are_stable_and_vary() {
        assert_eq!(look("sh"), look("sh"));
        let names = [
            "sh",
            "sleep",
            "cat",
            "acme",
            "ls",
            "draw-faces",
            "styxlisten",
        ];
        let looks: Vec<Look> = names.iter().map(|n| look(n)).collect();
        assert!(looks.iter().any(|l| l.hue != looks[0].hue));
        assert!(
            looks
                .iter()
                .all(|l| l.hue < 24 && (-2..=2).contains(&l.tall))
        );
    }

    #[test]
    fn colours() {
        assert_eq!(hsv(0.0, 1.0, 1.0), 0xFF0000FF);
        assert_eq!(hsv(120.0, 1.0, 1.0), 0x00FF00FF);
        assert_eq!(hsv(240.0, 1.0, 1.0), 0x0000FFFF);
        assert_eq!(hsv(0.0, 0.0, 1.0), 0xFFFFFFFF);
    }
}
