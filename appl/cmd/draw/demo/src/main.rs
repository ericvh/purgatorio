//! draw/demo: draws rectangles, lines, polygons, ellipses, arcs and text
//! on the screen, then follows the pointer. Holding button 1 paints;
//! button 3 ends the demo. When the window is resized the scene is drawn
//! again at the new size.
//!
//! `draw/demo -n` draws the scene once and exits, which works without a
//! window too (the emu's screen is then in memory).

use purgatorio::draw::{
    Display, ENDARROW, ENDDISC, ENDSQUARE, Font, Image, Mouse, Op, Point, Rect, WHITE,
};
use purgatorio::{Context, Error};

/// The pointer's marker: a disc of this radius.
const MARK: i32 = 6;

struct Colours {
    black: Image,
    grey: Image,
    red: Image,
    green: Image,
    blue: Image,
    yellow: Image,
    veil: Image,
}

/// The fonts the scene's text is in.
struct Fonts {
    sans: Font,
    mono: Font,
    big: Font,
}

fn scene(d: &mut Display, dst: &Image, c: &Colours, f: &mut Fonts) -> Result<(), Error> {
    let r = dst.r;
    let (w, h) = (r.dx(), r.dy());
    let at = |x: i32, y: i32| Point::new(r.min.x + x * w / 100, r.min.y + y * h / 100);
    let rect = |x0, y0, x1, y1| Rect {
        min: at(x0, y0),
        max: at(x1, y1),
    };
    let zero = Point::new(0, 0);
    // Colour bars along the top.
    let bars = [&c.red, &c.green, &c.blue, &c.yellow, &c.grey, &c.black];
    for (i, col) in bars.iter().enumerate() {
        let x = 5 + 15 * i as i32;
        d.draw(dst, rect(x, 4, x + 13, 14), col, None, zero)?;
    }
    // Overlapping rectangles, the top one translucent.
    d.draw(dst, rect(5, 20, 35, 45), &c.blue, None, zero)?;
    d.draw(dst, rect(20, 30, 50, 55), &c.veil, None, zero)?;
    // Lines with each end style and thickness.
    for (i, (e0, e1, t)) in [
        (ENDSQUARE, ENDSQUARE, 0),
        (ENDSQUARE, ENDSQUARE, 2),
        (ENDDISC, ENDDISC, 4),
        (ENDSQUARE, ENDARROW, 2),
    ]
    .into_iter()
    .enumerate()
    {
        let y = 62 + 8 * i as i32;
        d.line(dst, at(6, y), at(45, y + 4), e0, e1, t, &c.black, zero)?;
    }
    // Ellipses: an outline, a thick outline, a filled one and arcs.
    d.ellipse(dst, at(65, 30), w / 10, h / 10, 0, &c.black, zero)?;
    d.ellipse(dst, at(85, 30), w / 14, h / 9, 3, &c.red, zero)?;
    d.fill_ellipse(dst, at(65, 60), w / 12, h / 14, &c.green, zero)?;
    d.fill_arc(dst, at(85, 60), w / 14, w / 14, &c.yellow, zero, 30, 300)?;
    d.arc(dst, at(85, 60), w / 12, w / 12, 1, &c.black, zero, 30, 300)?;
    // A star, filled by the odd-even rule, and its outline.
    let star: Vec<Point> = [(60, 75), (68, 98), (52, 82), (72, 82), (56, 98)]
        .iter()
        .map(|&(x, y)| at(x, y))
        .collect();
    d.fill_poly(dst, &star, 1, &c.blue, zero)?;
    let mut closed = star.clone();
    closed.push(star[0]);
    d.poly(dst, &closed, ENDSQUARE, ENDSQUARE, 0, &c.black, zero)?;
    // Text: a title, a line of code over a background, and runes from
    // Go's coverage and from Unifont's.
    let title = "Purgatorio draw/demo";
    let tw = f.big.string_width(title);
    let tp = Point::new(r.max.x - tw - 8, at(0, 13).y);
    d.string(dst, tp, &c.black, zero, &mut f.big, title)?;
    let code = "fn main() { draw(\"hello\"); }";
    let cp = at(5, 15);
    d.string_bg(dst, cp, &c.black, zero, &mut f.mono, code, &c.yellow, zero)?;
    let mut p = at(5, 56);
    for s in ["Ελληνικά, Кириллица, ", "中文, 日本語, 한국어 ⌘"] {
        p = d.string(dst, p, &c.blue, zero, &mut f.sans, s)?;
    }
    Ok(())
}

/// Draws the scene into a new image of the screen's size and copies it
/// to the screen.
fn redraw(d: &mut Display, c: &Colours, f: &mut Fonts) -> Result<Image, Error> {
    let screen = d.screen();
    let back = d.alloc(screen.r, screen.chan, false, WHITE)?;
    scene(d, &back, c, f)?;
    d.gendraw_op(
        &screen,
        screen.r,
        &back,
        screen.r.min,
        None,
        Point::new(0, 0),
        Op::S,
    )?;
    d.flush()?;
    Ok(back)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let once = args.get(1).is_some_and(|a| a == "-n");
    let mut d = Display::open()?;
    let screen = d.screen();
    let colours = Colours {
        black: d.color(0x000000FF)?,
        grey: d.color(0x808080FF)?,
        red: d.color(0xDD2222FF)?,
        green: d.color(0x22AA44FF)?,
        blue: d.color(0x2255DDFF)?,
        yellow: d.color(0xEECC22FF)?,
        // Premultiplied: half-transparent orange.
        veil: d.color(0x7F400080)?,
    };
    let mut fonts = Fonts {
        sans: Font::default_font()?,
        mono: Font::open("/fonts/mono/unicode.13.font")?,
        big: Font::open("/fonts/sans/unicode.20.font")?,
    };
    // The scene is drawn into an image of the screen's size, so that the
    // pointer's marker can be erased by copying it back.
    let mut back = redraw(&mut d, &colours, &mut fonts)?;
    if once {
        return Ok(());
    }
    let mut screen = screen;
    let mut mouse = Mouse::open()?;
    let mut last: Option<Rect> = None;
    loop {
        let m = mouse.read()?;
        if m.buttons & 4 != 0 {
            return Ok(());
        }
        // The window sends a pointer event when it is resized.
        if d.check_resize()? {
            d.free(back)?;
            back = redraw(&mut d, &colours, &mut fonts)?;
            screen = d.screen();
            last = None;
        }
        if m.buttons & 1 != 0 {
            d.fill_ellipse(&back, m.xy, 3, 3, &colours.red, Point::new(0, 0))?;
        }
        let mark = Rect::new(
            m.xy.x - MARK - 1,
            m.xy.y - MARK - 1,
            m.xy.x + MARK + 2,
            m.xy.y + MARK + 2,
        );
        for r in last.iter().chain(std::iter::once(&mark)) {
            d.gendraw_op(&screen, *r, &back, r.min, None, Point::new(0, 0), Op::S)?;
        }
        d.fill_ellipse(&screen, m.xy, MARK, MARK, &colours.yellow, Point::new(0, 0))?;
        d.ellipse(
            &screen,
            m.xy,
            MARK,
            MARK,
            0,
            &colours.black,
            Point::new(0, 0),
        )?;
        d.flush()?;
        last = Some(mark);
    }
}
