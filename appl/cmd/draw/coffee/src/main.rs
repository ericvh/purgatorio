//! draw/coffee: a steaming cup of coffee, in the spirit of Inferno's
//! `wm/coffee`. Inferno's shows a sequence of pictures; this one draws
//! its own scene with draw(2)'s polygons and ellipses (a wall, a table,
//! a saucer and a cup) and animates the steam: three ribbons that sway
//! and fade as they rise, each band a translucent polygon.
//!
//! The still part of the scene is drawn once into an image of the
//! screen's size. Each frame copies back only the steam's rectangle,
//! draws the ribbons over it and flushes, so a frame damages a small
//! part of the screen. The program sleeps between frames in
//! `purgatorio::ready` (about 25 a second), waking early for the pointer
//! or a key. It follows a resize by checking the screen's size on each
//! pointer event and a few times a second. `q` or Delete ends it.

use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use purgatorio::draw::{Display, ENDSQUARE, Image, Keyboard, Mouse, Op, Point, Rect, WHITE};
use purgatorio::{Context, Error, Ready};

/// Time between frames.
const FRAME: Duration = Duration::from_millis(40);
/// Frames between checks of the screen's size.
const SIZE_CHECK: u32 = 10;
/// Points along each ribbon of steam.
const STEPS: usize = 24;
/// The steam's opacity by height, bottom to top (0-255).
const FADE: [u32; 7] = [0x70, 0x60, 0x50, 0x40, 0x30, 0x20, 0x10];

/// Where the cup is on a screen of rectangle `r`, in units of a
/// hundredth of the smaller side.
struct Layout {
    r: Rect,
    u: f64,
    cx: i32,
    table: i32,
    rim: i32,
    base: i32,
}

impl Layout {
    fn new(r: Rect) -> Layout {
        let u = f64::from(r.dx().min(r.dy()).max(40)) / 100.0;
        let table = r.min.y + r.dy() * 78 / 100;
        Layout {
            r,
            u,
            cx: r.min.x + r.dx() / 2,
            table,
            rim: table - (34.0 * u) as i32,
            base: table - (6.0 * u) as i32,
        }
    }

    /// `n` units, in pixels.
    fn px(&self, n: f64) -> i32 {
        (n * self.u).round() as i32
    }

    /// The rectangle the steam is drawn in.
    fn steam(&self) -> Rect {
        let top = (self.rim - self.px(62.0)).max(self.r.min.y);
        Rect::new(
            self.cx - self.px(30.0),
            top,
            self.cx + self.px(30.0),
            self.rim - self.px(3.0),
        )
    }
}

struct Paint {
    colours: Vec<(u32, Image)>,
}

impl Paint {
    /// An image of colour `rgba`, allocated once.
    fn get(&mut self, d: &mut Display, rgba: u32) -> Result<Image, Error> {
        if let Some((_, i)) = self.colours.iter().find(|(c, _)| *c == rgba) {
            return Ok(i.clone());
        }
        let i = d.color(rgba)?;
        self.colours.push((rgba, i.clone()));
        Ok(i)
    }
}

/// Mixes two `0xRRGGBBAA` colours, `t` of the way from `a` to `b`.
fn mix(a: u32, b: u32, t: f64) -> u32 {
    let mut out = 0xFF;
    for shift in [24, 16, 8] {
        let x = f64::from((a >> shift) & 0xFF);
        let y = f64::from((b >> shift) & 0xFF);
        out |= ((x + (y - x) * t).round() as u32 & 0xFF) << shift;
    }
    out
}

/// The still scene: wall, table, saucer and cup.
fn scene(d: &mut Display, p: &mut Paint, dst: &Image, l: &Layout) -> Result<(), Error> {
    let zero = Point::new(0, 0);
    let r = l.r;
    // The wall, light at the top and warmer towards the table.
    let bands = 24;
    let wall = l.table - r.min.y;
    for i in 0..bands {
        let y0 = r.min.y + wall * i / bands;
        let y1 = r.min.y + wall * (i + 1) / bands;
        let c = p.get(
            d,
            mix(0xF6E9D2FF, 0xDDBF93FF, f64::from(i) / f64::from(bands)),
        )?;
        d.draw(dst, Rect::new(r.min.x, y0, r.max.x, y1), &c, None, zero)?;
    }
    let wood = p.get(d, 0x8A5530FF)?;
    let edge = p.get(d, 0xA86C3EFF)?;
    let dark = p.get(d, 0x6B3F22FF)?;
    d.draw(
        dst,
        Rect::new(r.min.x, l.table, r.max.x, r.max.y),
        &wood,
        None,
        zero,
    )?;
    d.draw(
        dst,
        Rect::new(r.min.x, l.table, r.max.x, l.table + l.px(1.5).max(1)),
        &edge,
        None,
        zero,
    )?;
    // A few grain lines in the wood.
    for (i, f) in [0.35, 0.6, 0.85].into_iter().enumerate() {
        let y = l.table + ((r.max.y - l.table) as f64 * f) as i32;
        let x0 = r.min.x + r.dx() * (i as i32 * 13 % 40) / 100;
        d.line(
            dst,
            Point::new(x0, y),
            Point::new(x0 + r.dx() / 3, y),
            ENDSQUARE,
            ENDSQUARE,
            0,
            &dark,
            zero,
        )?;
    }
    let (cx, u) = (l.cx, |n| l.px(n));
    // The saucer and its shadow.
    let shadow = p.get(d, 0x5E3A1EFF)?;
    let china = p.get(d, 0xFBFAF6FF)?;
    let shade = p.get(d, 0xE4E1D8FF)?;
    let line = p.get(d, 0xB9B3A6FF)?;
    let sy = l.table - u(2.0);
    d.fill_ellipse(
        dst,
        Point::new(cx + u(2.0), sy + u(3.0)),
        u(35.0),
        u(7.0),
        &shadow,
        zero,
    )?;
    d.fill_ellipse(dst, Point::new(cx, sy), u(34.0), u(7.0), &china, zero)?;
    d.ellipse(dst, Point::new(cx, sy), u(34.0), u(7.0), 0, &line, zero)?;
    d.ellipse(
        dst,
        Point::new(cx, sy - u(1.0)),
        u(22.0),
        u(4.0),
        0,
        &shade,
        zero,
    )?;
    // The handle, under the body so the body hides its inner half.
    let hc = Point::new(cx + u(20.0), (l.rim + l.base) / 2 - u(2.0));
    d.ellipse(dst, hc, u(9.0), u(10.0), u(2.6).max(1), &line, zero)?;
    d.ellipse(dst, hc, u(9.0), u(10.0), u(1.8).max(1), &china, zero)?;
    // The body: a tapering cup with a rounded base, shaded on the right,
    // and a band of colour.
    let (wr, wb) = (u(20.0), u(15.0));
    let body = [
        Point::new(cx - wr, l.rim),
        Point::new(cx + wr, l.rim),
        Point::new(cx + wb, l.base),
        Point::new(cx - wb, l.base),
    ];
    d.fill_ellipse(dst, Point::new(cx, l.base), wb, u(4.0), &china, zero)?;
    d.fill_poly(dst, &body, !0, &china, zero)?;
    let right = [
        Point::new(cx + wr * 11 / 20, l.rim),
        Point::new(cx + wr, l.rim),
        Point::new(cx + wb, l.base),
        Point::new(cx + wb * 11 / 20, l.base),
    ];
    d.fill_poly(dst, &right, !0, &shade, zero)?;
    d.fill_arc(
        dst,
        Point::new(cx, l.base),
        wb,
        u(4.0),
        &shade,
        zero,
        303,
        57,
    )?;
    let width_at = |y: i32| wr - (wr - wb) * (y - l.rim) / (l.base - l.rim).max(1);
    let (b0, b1) = (l.rim + u(9.0), l.rim + u(14.0));
    let band = [
        Point::new(cx - width_at(b0), b0),
        Point::new(cx + width_at(b0), b0),
        Point::new(cx + width_at(b1), b1),
        Point::new(cx - width_at(b1), b1),
    ];
    let red = p.get(d, 0xC0432CFF)?;
    d.fill_poly(dst, &band, !0, &red, zero)?;
    d.poly(
        dst,
        &[body[1], body[2]],
        ENDSQUARE,
        ENDSQUARE,
        0,
        &line,
        zero,
    )?;
    d.poly(
        dst,
        &[body[0], body[3]],
        ENDSQUARE,
        ENDSQUARE,
        0,
        &line,
        zero,
    )?;
    d.arc(
        dst,
        Point::new(cx, l.base),
        wb,
        u(4.0),
        0,
        &line,
        zero,
        180,
        180,
    )?;
    // The rim and the coffee in it.
    let coffee = p.get(d, 0x4A2A14FF)?;
    let crema = p.get(d, 0x8A5A32FF)?;
    d.fill_ellipse(dst, Point::new(cx, l.rim), wr, u(4.5), &china, zero)?;
    d.ellipse(dst, Point::new(cx, l.rim), wr, u(4.5), 0, &line, zero)?;
    d.fill_ellipse(
        dst,
        Point::new(cx, l.rim + u(0.6)),
        wr - u(2.0),
        u(3.2),
        &coffee,
        zero,
    )?;
    d.fill_ellipse(
        dst,
        Point::new(cx - u(5.0), l.rim + u(0.4)),
        u(6.0),
        u(1.0).max(1),
        &crema,
        zero,
    )?;
    Ok(())
}

/// Draws the steam at time `t` seconds.
fn steam(d: &mut Display, p: &mut Paint, dst: &Image, l: &Layout, t: f64) -> Result<(), Error> {
    let zero = Point::new(0, 0);
    let s = l.steam();
    let height = f64::from(s.dy() - l.px(2.0));
    let bottom = f64::from(s.max.y - l.px(1.0));
    for (j, (dx, phase)) in [(-8.0, 0.0), (0.5, 2.1), (8.5, 4.2)]
        .into_iter()
        .enumerate()
    {
        // Each ribbon breathes: its top rises and falls.
        let reach = 0.78 + 0.22 * (t * 0.9 + phase).sin();
        let speed = 2.2 + 0.3 * j as f64;
        let edge = |v: f64| -> (f64, f64, f64) {
            let x = f64::from(l.cx)
                + dx * l.u
                + (2.0 + 7.5 * v) * l.u * (TAU * 1.3 * v - speed * t + phase).sin()
                + 3.0 * l.u * v * (t * 0.5 + phase).sin();
            let y = bottom - v * height * reach;
            let w = ((3.2 - 1.6 * v) * l.u).max(1.0);
            (x, y, w)
        };
        let pts: Vec<(f64, f64, f64)> = (0..STEPS)
            .map(|k| edge(k as f64 / (STEPS - 1) as f64))
            .collect();
        // One polygon per band of the fade, so no part is drawn twice.
        let per = STEPS.div_ceil(FADE.len());
        for (b, &alpha) in FADE.iter().enumerate() {
            let lo = b * per;
            let hi = ((b + 1) * per).min(STEPS - 1);
            if lo >= hi {
                break;
            }
            let band = &pts[lo..=hi];
            let mut poly: Vec<Point> = band
                .iter()
                .map(|&(x, y, w)| Point::new((x - w).round() as i32, y.round() as i32))
                .collect();
            poly.extend(
                band.iter()
                    .rev()
                    .map(|&(x, y, w)| Point::new((x + w).round() as i32, y.round() as i32)),
            );
            // Premultiplied white at `alpha`.
            let c = p.get(d, (alpha * 0x0101_0100) | alpha)?;
            d.fill_poly(dst, &poly, !0, &c, zero)?;
        }
    }
    Ok(())
}

struct Coffee {
    d: Display,
    p: Paint,
    l: Layout,
    still: Image,
}

impl Coffee {
    /// Draws the still scene for the screen's size and shows it.
    fn layout(d: &mut Display, p: &mut Paint) -> Result<(Layout, Image), Error> {
        let screen = d.screen();
        let l = Layout::new(screen.r);
        let still = d.alloc(screen.r, screen.chan, false, WHITE)?;
        scene(d, p, &still, &l)?;
        let zero = Point::new(0, 0);
        d.gendraw_op(&screen, screen.r, &still, screen.r.min, None, zero, Op::S)?;
        Ok((l, still))
    }

    fn frame(&mut self, t: f64) -> Result<(), Error> {
        let screen = self.d.screen();
        let s = self.l.steam();
        let zero = Point::new(0, 0);
        self.d
            .gendraw_op(&screen, s, &self.still, s.min, None, zero, Op::S)?;
        steam(&mut self.d, &mut self.p, &screen, &self.l, t)?;
        self.d.flush()
    }

    fn resize(&mut self) -> Result<(), Error> {
        let old = self.still.clone();
        self.d.free(old)?;
        let (l, still) = Coffee::layout(&mut self.d, &mut self.p)?;
        self.l = l;
        self.still = still;
        Ok(())
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() > 1 {
        return Err(Error::from("usage: draw/coffee"));
    }
    let mut d = Display::open()?;
    let mut p = Paint {
        colours: Vec::new(),
    };
    let (l, still) = Coffee::layout(&mut d, &mut p)?;
    let mut c = Coffee { d, p, l, still };
    let mut mouse = Mouse::open()?;
    let mut kbd = Keyboard::open().ok();
    let start = Instant::now();
    let mut next = Instant::now();
    let mut frames = 0u32;
    loop {
        let now = Instant::now();
        if now >= next {
            frames += 1;
            if frames.is_multiple_of(SIZE_CHECK) && c.d.check_resize()? {
                c.resize()?;
            }
            c.frame(start.elapsed().as_secs_f64())?;
            next += FRAME;
            if next < now {
                // Fell behind (a busy host): start counting again.
                next = now + FRAME;
            }
            continue;
        }
        if let Some(k) = &mut kbd
            && k.pending()
        {
            if matches!(k.read()?, 'q' | '\x7F') {
                return Ok(());
            }
            continue;
        }
        let ms = (next - now).as_millis().clamp(1, 1000) as i32;
        let mut fds = vec![Ready::Read(mouse.fd())];
        if let Some(k) = &kbd {
            fds.push(Ready::Read(k.fd()));
        }
        match purgatorio::ready(&fds, ms)? {
            Some(0) => {
                mouse.read()?;
                if c.d.check_resize()? {
                    c.resize()?;
                }
            }
            Some(_) => {
                if let Some(k) = &mut kbd
                    && matches!(k.read()?, 'q' | '\x7F')
                {
                    return Ok(());
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing() {
        assert_eq!(mix(0x000000FF, 0xFFFFFFFF, 0.0), 0x000000FF);
        assert_eq!(mix(0x000000FF, 0xFFFFFFFF, 1.0), 0xFFFFFFFF);
        assert_eq!(mix(0x204060FF, 0x406080FF, 0.5), 0x305070FF);
    }

    #[test]
    fn steam_is_above_the_cup() {
        let l = Layout::new(Rect::new(0, 0, 640, 480));
        let s = l.steam();
        assert!(s.max.y < l.rim && s.min.y >= 0 && s.dx() > 0 && s.dy() > 0);
    }
}
