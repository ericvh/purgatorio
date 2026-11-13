//! draw/clock: a clock on `/dev/draw`, after Inferno's `wm/clock` and
//! Plan 9's `clock`, drawn here with draw(2)'s lines, ellipses and
//! polygons and no toolkit.
//!
//! `draw/clock [-d] [-t seconds]` fills the screen (or its window) with
//! an analog face: sixty ticks, numerals when there is room, hour, minute
//! and second hands, and the date. `-d` shows a seven-segment
//! `HH:MM:SS` instead. The time is GMT, as `date` prints it: there is no
//! timezone database yet. `-t seconds` stops the clock at that many
//! seconds after the epoch, for tests and screenshots.
//!
//! The clock wakes at each second and redraws; it follows a resize (the
//! emu's window, or a window manager's) by checking the screen's size on
//! every pointer event and every tick. `q` or Delete ends it.

use std::time::{SystemTime, UNIX_EPOCH};

use purgatorio::arg::Arg;
use purgatorio::daytime::{self, Tm};
use purgatorio::draw::{
    Display, ENDDISC, ENDSQUARE, Font, Image, Keyboard, Mouse, Op, Point, WHITE,
};
use purgatorio::{Context, Error, Ready};

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The segments lit for each digit: bit 0 is the top (a), then
/// clockwise b to f, and bit 6 the middle (g).
const SEGMENTS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

struct Colours {
    back: Image,
    face: Image,
    rim: Image,
    ink: Image,
    second: Image,
    lcd: Image,
    lit: Image,
    unlit: Image,
}

struct Clock {
    d: Display,
    c: Colours,
    small: Font,
    big: Font,
    digital: bool,
}

/// The point `r` from `c` at `deg` degrees clockwise from twelve.
fn polar(c: Point, r: f64, deg: f64) -> Point {
    let a = deg.to_radians();
    Point::new(
        c.x + (r * a.sin()).round() as i32,
        c.y - (r * a.cos()).round() as i32,
    )
}

/// The polygon of one segment from `p0` to `p1` (horizontal or
/// vertical), `k` pixels each side of the line, with pointed ends.
fn segment(p0: Point, p1: Point, k: i32) -> Vec<Point> {
    let gap = (k / 2).max(1);
    if p0.y == p1.y {
        let (x0, x1, y) = (p0.x + gap, p1.x - gap, p0.y);
        vec![
            Point::new(x0, y),
            Point::new(x0 + k, y - k),
            Point::new(x1 - k, y - k),
            Point::new(x1, y),
            Point::new(x1 - k, y + k),
            Point::new(x0 + k, y + k),
        ]
    } else {
        let (y0, y1, x) = (p0.y + gap, p1.y - gap, p0.x);
        vec![
            Point::new(x, y0),
            Point::new(x + k, y0 + k),
            Point::new(x + k, y1 - k),
            Point::new(x, y1),
            Point::new(x - k, y1 - k),
            Point::new(x - k, y0 + k),
        ]
    }
}

fn date(tm: &Tm) -> String {
    format!(
        "{} {} {} {}",
        DAYS[tm.wday as usize % 7],
        tm.mday,
        MONTHS[tm.mon as usize % 12],
        tm.year
    )
}

impl Clock {
    fn draw(&mut self, t: i64) -> Result<(), Error> {
        let screen = self.d.screen();
        let tm = daytime::gmt(t);
        // Drawn off screen and copied, so the screen never shows a
        // half-drawn face.
        let back = self.d.alloc(screen.r, screen.chan, false, WHITE)?;
        if self.digital {
            self.lcd(&back, &tm)?;
        } else {
            self.analog(&back, &tm)?;
        }
        let zero = Point::new(0, 0);
        self.d
            .gendraw_op(&screen, screen.r, &back, screen.r.min, None, zero, Op::S)?;
        self.d.free(back)?;
        self.d.flush()
    }

    fn analog(&mut self, dst: &Image, tm: &Tm) -> Result<(), Error> {
        let d = &mut self.d;
        let c = &self.c;
        let zero = Point::new(0, 0);
        let r = dst.r;
        d.draw(dst, r, &c.back, None, zero)?;
        let ctr = Point::new(r.min.x + r.dx() / 2, r.min.y + r.dy() / 2);
        let rad = (r.dx().min(r.dy()) / 2 - 6).max(4);
        let rf = f64::from(rad);
        d.fill_ellipse(dst, ctr, rad, rad, &c.face, zero)?;
        d.ellipse(dst, ctr, rad, rad, (rad / 40).max(1), &c.rim, zero)?;
        for i in 0..60 {
            let hour = i % 5 == 0;
            let inner = if hour { 0.82 } else { 0.88 };
            let thick = if hour { (rad / 60).max(1) } else { 0 };
            let deg = f64::from(i) * 6.0;
            d.line(
                dst,
                polar(ctr, rf * inner, deg),
                polar(ctr, rf * 0.93, deg),
                ENDSQUARE,
                ENDSQUARE,
                thick,
                &c.ink,
                zero,
            )?;
        }
        if rad >= 110 {
            for h in 1..=12 {
                let s = h.to_string();
                let w = self.big.string_width(&s);
                let p = polar(ctr, rf * 0.70, f64::from(h) * 30.0);
                let at = Point::new(p.x - w / 2, p.y - self.big.height / 2);
                d.string(dst, at, &c.ink, zero, &mut self.big, &s)?;
            }
        }
        if rad >= 60 {
            for (s, y) in [
                (date(tm), ctr.y + rad * 2 / 5),
                (
                    String::from("GMT"),
                    ctr.y - rad * 3 / 10 - self.small.height,
                ),
            ] {
                let w = self.small.string_width(&s);
                let at = Point::new(ctr.x - w / 2, y);
                d.string(dst, at, &c.rim, zero, &mut self.small, &s)?;
            }
        }
        let (h, m, s) = (f64::from(tm.hour), f64::from(tm.min), f64::from(tm.sec));
        let hands = [
            (
                (h % 12.0 + m / 60.0) * 30.0,
                0.50,
                (rad / 22).max(1),
                &c.ink,
            ),
            ((m + s / 60.0) * 6.0, 0.78, (rad / 36).max(1), &c.ink),
        ];
        for (deg, len, thick, col) in hands {
            d.line(
                dst,
                polar(ctr, rf * 0.08, deg + 180.0),
                polar(ctr, rf * len, deg),
                ENDDISC,
                ENDDISC,
                thick,
                col,
                zero,
            )?;
        }
        let deg = s * 6.0;
        d.line(
            dst,
            polar(ctr, rf * 0.18, deg + 180.0),
            polar(ctr, rf * 0.88, deg),
            ENDSQUARE,
            ENDDISC,
            (rad / 120).max(0),
            &c.second,
            zero,
        )?;
        let hub = (rad / 18).max(2);
        d.fill_ellipse(dst, ctr, hub, hub, &c.ink, zero)?;
        d.fill_ellipse(dst, ctr, hub / 2, hub / 2, &c.second, zero)?;
        Ok(())
    }

    fn lcd(&mut self, dst: &Image, tm: &Tm) -> Result<(), Error> {
        let d = &mut self.d;
        let c = &self.c;
        let zero = Point::new(0, 0);
        let r = dst.r;
        d.draw(dst, r, &c.lcd, None, zero)?;
        // Six digits and two colons half a digit wide, with a quarter
        // digit between each: 9.75 digit widths across, and a digit is
        // twice as high as wide.
        let dw = (r.dx() * 4 / 41).min(r.dy() * 3 / 10).max(6);
        let dh = 2 * dw;
        let k = (dw / 10).max(1);
        let total = dw * 39 / 4;
        let mut x = r.min.x + (r.dx() - total) / 2;
        let y = r.min.y + (r.dy() - dh) / 2 - self.small.height / 2;
        let text = format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec);
        for ch in text.chars() {
            if ch == ':' {
                let cx = x + dw / 4;
                for cy in [y + dh * 3 / 10, y + dh * 7 / 10] {
                    d.fill_ellipse(dst, Point::new(cx, cy), k, k, &c.lit, zero)?;
                }
                x += dw / 2 + dw / 4;
                continue;
            }
            let bits = SEGMENTS[ch.to_digit(10).unwrap_or(8) as usize];
            let (l, rt) = (x + k, x + dw - k);
            let (top, mid, bot) = (y + k, y + dh / 2, y + dh - k);
            let segs = [
                (Point::new(l, top), Point::new(rt, top)),
                (Point::new(rt, top), Point::new(rt, mid)),
                (Point::new(rt, mid), Point::new(rt, bot)),
                (Point::new(l, bot), Point::new(rt, bot)),
                (Point::new(l, mid), Point::new(l, bot)),
                (Point::new(l, top), Point::new(l, mid)),
                (Point::new(l, mid), Point::new(rt, mid)),
            ];
            for (i, (p0, p1)) in segs.into_iter().enumerate() {
                let col = if bits & (1 << i) != 0 {
                    &c.lit
                } else {
                    &c.unlit
                };
                d.fill_poly(dst, &segment(p0, p1, k), !0, col, zero)?;
            }
            x += dw + dw / 4;
        }
        let s = format!("{} GMT", date(tm));
        let w = self.small.string_width(&s);
        let at = Point::new(r.min.x + (r.dx() - w) / 2, y + dh + k * 3);
        d.string(dst, at, &c.lit, zero, &mut self.small, &s)?;
        Ok(())
    }
}

/// Seconds and milliseconds since the epoch.
fn now() -> (i64, u32) {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or((0, 0), |d| (d.as_secs() as i64, d.subsec_millis()))
}

enum Event {
    Tick,
    Pointer,
    Key(char),
}

/// Waits up to `ms` milliseconds for the pointer or a key.
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
        Some(0) => {
            mouse.read()?;
            Ok(Event::Pointer)
        }
        Some(_) => match kbd {
            Some(k) => Ok(Event::Key(k.read()?)),
            None => Ok(Event::Tick),
        },
        None => Ok(Event::Tick),
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "draw/clock [-d] [-t seconds]");
    let (mut digital, mut fixed) = (false, None);
    while let Some(c) = arg.opt() {
        match c {
            'd' => digital = true,
            't' => {
                let v = arg.earg()?;
                fixed = Some(v.parse::<i64>().map_err(|_| arg.usage())?);
            }
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error::from("usage: draw/clock [-d] [-t seconds]"));
    }
    let mut d = Display::open()?;
    let c = Colours {
        back: d.color(0xE8E4D8FF)?,
        face: d.color(0xFFFFF8FF)?,
        rim: d.color(0x44464CFF)?,
        ink: d.color(0x1C1E26FF)?,
        second: d.color(0xCC2A1EFF)?,
        lcd: d.color(0x141814FF)?,
        lit: d.color(0x4CE07AFF)?,
        unlit: d.color(0x1F2E22FF)?,
    };
    let mut clock = Clock {
        d,
        c,
        small: Font::open("/fonts/sans/unicode.13.font")?,
        big: Font::open("/fonts/sans/unicode.20.font")?,
        digital,
    };
    let mut mouse = Mouse::open()?;
    let mut kbd = Keyboard::open().ok();
    let mut shown = fixed.unwrap_or_else(|| now().0);
    clock.draw(shown)?;
    loop {
        let ms = 1000 - now().1 as i32;
        let resized = match next(&mut mouse, &mut kbd, ms)? {
            Event::Key('q' | '\x7F') => return Ok(()),
            Event::Key(_) => false,
            Event::Pointer | Event::Tick => clock.d.check_resize()?,
        };
        let t = fixed.unwrap_or_else(|| now().0);
        if resized || t != shown {
            shown = t;
            clock.draw(t)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_point_the_right_way() {
        let c = Point::new(100, 100);
        assert_eq!(polar(c, 50.0, 0.0), Point::new(100, 50));
        assert_eq!(polar(c, 50.0, 90.0), Point::new(150, 100));
        assert_eq!(polar(c, 50.0, 180.0), Point::new(100, 150));
        assert_eq!(polar(c, 50.0, 270.0), Point::new(50, 100));
    }

    #[test]
    fn dates() {
        // 2026-11-13 12:34:56 GMT, a Friday.
        assert_eq!(date(&daytime::gmt(1_794_573_296)), "Fri 13 Nov 2026");
    }

    #[test]
    fn segments_close() {
        let s = segment(Point::new(0, 10), Point::new(40, 10), 4);
        assert_eq!(s.len(), 6);
        assert!(s.iter().all(|p| (6..=14).contains(&p.y)));
        let v = segment(Point::new(5, 0), Point::new(5, 40), 4);
        assert!(v.iter().all(|p| (1..=9).contains(&p.x)));
    }
}
