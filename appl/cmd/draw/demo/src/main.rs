//! draw/demo: draws rectangles, lines, polygons, ellipses and arcs on the
//! screen, then follows the pointer. Holding button 1 paints; button 3
//! ends the demo.
//!
//! `draw/demo -n` draws the scene once and exits, which works without a
//! window too (the emu's screen is then in memory).

use purgatorio::draw::{
    Display, ENDARROW, ENDDISC, ENDSQUARE, Image, Mouse, Op, Point, Rect, WHITE,
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

fn scene(d: &mut Display, dst: &Image, c: &Colours) -> Result<(), Error> {
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
    Ok(())
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
    // The scene is drawn into an image of the screen's size, so that the
    // pointer's marker can be erased by copying it back.
    let back = d.alloc(screen.r, screen.chan, false, WHITE)?;
    scene(&mut d, &back, &colours)?;
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
    if once {
        return Ok(());
    }
    let mut mouse = Mouse::open()?;
    let mut last: Option<Rect> = None;
    loop {
        let m = mouse.read()?;
        if m.buttons & 4 != 0 {
            return Ok(());
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
