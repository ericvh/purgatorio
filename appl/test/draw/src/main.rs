//! draw: checks `/dev/draw`, `/dev/pointer` and the SDK's draw module on
//! the screen the emu has without a window. Each check draws through the
//! device, reads the pixels back with `r`, and compares them with the
//! same drawing done here by memdraw, plus a few pixels known by hand.
//! Text goes through the device's font cache (`i`, `l`, `s`, `x`) and is
//! compared with glyphs drawn here straight from the subfont files.
//! Prints `ok <name>` per passing check and `FAIL <name>: <why>`
//! otherwise; exits with status `fail:draw` if any check failed.
//!
//! `test/draw -d` checks nothing: it writes the screen's width and
//! height as a line of text and then its pixels, rows of `x8r8g8b8`, to
//! standard output, for the tests that compare screens. It writes the
//! whole display even when run in a window.

use std::fs::OpenOptions;
use std::io::Write;

use purgatorio::draw::memdraw::{self, FontFile, Image as MemImage, Subfont};
use purgatorio::draw::{
    Display, ENDARROW, ENDDISC, ENDSQUARE, Font, GREY8, Mouse, NCACHE, Op, Point, RGB24, Rect,
    XRGB32,
};
use purgatorio::{Context, Error};

type Check = fn() -> Result<(), String>;

fn e(err: Error) -> String {
    err.0
}

/// A local image like one the device has: `chan`, `r`, filled with
/// `rgba`.
fn local(r: Rect, chan: memdraw::Chan, rgba: u32) -> MemImage {
    let mut i = MemImage::new(r, chan).unwrap();
    i.fill(memdraw::rgba(rgba));
    i
}

fn solid(chan: memdraw::Chan, rgba: u32) -> MemImage {
    let mut i = local(Rect::new(0, 0, 1, 1), chan, rgba);
    i.repl = true;
    i.clipr = Rect::infinite();
    i
}

fn screen_info() -> Result<(), String> {
    let d = Display::open().map_err(e)?;
    let s = d.screen();
    if s.id != 0 || s.chan != XRGB32 || s.r != Rect::new(0, 0, 640, 480) || s.clipr != s.r {
        return Err(format!("screen is {s:?}"));
    }
    let dir: Vec<String> = std::fs::read_dir("/dev/draw")
        .map_err(|x| x.to_string())?
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    if !dir.contains(&String::from("new")) || !dir.contains(&format!("{}", d.conn)) {
        return Err(format!("/dev/draw holds {dir:?}"));
    }
    Ok(())
}

fn ctl_info() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let i = d
        .alloc(Rect::new(-3, 2, 13, 9), GREY8, false, 0x404040FF)
        .map_err(e)?;
    d.send().map_err(e)?;
    let got = d.info(i.id).map_err(e)?;
    if got != i {
        return Err(format!("ctl says {got:?}, want {i:?}"));
    }
    // As devdraw, a read answers one request: read again without asking
    // and there is no image to describe.
    let mut ctl =
        std::fs::File::open(format!("/dev/draw/{}/ctl", d.conn)).map_err(|x| e(x.into()))?;
    match std::io::Read::read(&mut ctl, &mut [0u8; 144]).map_err(|x| e(x.into())) {
        Err(x) if x == "unknown id for draw image" => {}
        other => return Err(format!("a second read of ctl: {other:?}")),
    }
    match d.info(999) {
        Err(x) if x.0 == "unknown id for draw image" => Ok(()),
        other => Err(format!("info of a bad id: {other:?}")),
    }
}

/// Rectangles, a mask and an operator on an RGB24 image, compared with
/// memdraw.
fn draw_and_read_back() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let r = Rect::new(0, 0, 16, 12);
    let dst = d.alloc(r, RGB24, false, 0xFFFFFFFF).map_err(e)?;
    let red = d.color(0xFF0000FF).map_err(e)?;
    let half = d.color(0x00008080).map_err(e)?;
    let mask = d
        .alloc(Rect::new(0, 0, 4, 4), GREY8, true, 0x808080FF)
        .map_err(e)?;
    d.draw(&dst, Rect::new(2, 2, 10, 8), &red, None, Point::new(0, 0))
        .map_err(e)?;
    d.draw(&dst, Rect::new(6, 4, 14, 11), &half, None, Point::new(0, 0))
        .map_err(e)?;
    d.gendraw_op(
        &dst,
        Rect::new(0, 9, 16, 12),
        &red,
        Point::new(0, 0),
        Some(&mask),
        Point::new(0, 0),
        Op::S,
    )
    .map_err(e)?;
    let got = d.unload(&dst, r).map_err(e)?;

    let mut want = local(r, RGB24, 0xFFFFFFFF);
    let lred = solid(RGB24, 0xFF0000FF);
    let lhalf = solid(memdraw::RGBA32, 0x00008080);
    let mut lmask = local(Rect::new(0, 0, 4, 4), GREY8, 0x808080FF);
    lmask.repl = true;
    lmask.clipr = Rect::infinite();
    let z = Point::new(0, 0);
    memdraw::draw(
        &mut want,
        Rect::new(2, 2, 10, 8),
        &lred,
        z,
        None,
        z,
        Op::SOVER_D,
    );
    memdraw::draw(
        &mut want,
        Rect::new(6, 4, 14, 11),
        &lhalf,
        z,
        None,
        z,
        Op::SOVER_D,
    );
    memdraw::draw(
        &mut want,
        Rect::new(0, 9, 16, 12),
        &lred,
        z,
        Some(&lmask),
        z,
        Op::S,
    );
    if got != want.unload(r).unwrap() {
        return Err(String::from("pixels differ from memdraw's"));
    }
    // Pixel (3, 3) is red: little-endian b, g, r.
    let o = (3 * 16 + 3) * 3;
    if got[o..o + 3] != [0, 0, 0xFF] {
        return Err(format!("pixel (3,3) is {:?}", &got[o..o + 3]));
    }
    Ok(())
}

/// Lines, polygons, ellipses and arcs on a k8 image, compared with
/// memdraw: every message's encoding has to arrive intact.
fn shapes() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let r = Rect::new(0, 0, 64, 48);
    let dst = d.alloc(r, GREY8, false, 0x000000FF).map_err(e)?;
    let white = d.color(0xFFFFFFFF).map_err(e)?;
    let z = Point::new(0, 0);
    let p = Point::new;
    let star = [p(30, 2), (p(40, 40)), p(14, 16), p(46, 16), p(20, 40)];
    let far = [p(1, 1), p(300, 2), p(-200, 30)];
    d.line(&dst, p(2, 3), p(60, 20), ENDSQUARE, ENDSQUARE, 0, &white, z)
        .map_err(e)?;
    d.line(&dst, p(5, 40), p(50, 30), ENDDISC, ENDARROW, 1, &white, z)
        .map_err(e)?;
    d.poly(&dst, &star, ENDSQUARE, ENDDISC, 1, &white, z)
        .map_err(e)?;
    d.fill_poly(&dst, &star, 1, &white, z).map_err(e)?;
    d.fill_poly(&dst, &far, !0, &white, z).map_err(e)?;
    d.ellipse(&dst, p(12, 12), 9, 6, 0, &white, z).map_err(e)?;
    d.fill_ellipse(&dst, p(52, 36), 6, 8, &white, z)
        .map_err(e)?;
    d.arc(&dst, p(32, 24), 14, 14, 1, &white, z, 200, -120)
        .map_err(e)?;
    d.fill_arc(&dst, p(55, 8), 7, 7, &white, z, 0, 90)
        .map_err(e)?;
    let got = d.unload(&dst, r).map_err(e)?;

    let mut want = local(r, GREY8, 0x000000FF);
    let w = solid(GREY8, 0xFFFFFFFF);
    let op = Op::SOVER_D;
    memdraw::line(
        &mut want,
        p(2, 3),
        p(60, 20),
        ENDSQUARE,
        ENDSQUARE,
        0,
        &w,
        z,
        op,
    );
    memdraw::line(
        &mut want,
        p(5, 40),
        p(50, 30),
        ENDDISC,
        ENDARROW,
        1,
        &w,
        z,
        op,
    );
    memdraw::poly(&mut want, &star, ENDSQUARE, ENDDISC, 1, &w, z, op);
    memdraw::fill_poly(&mut want, &star, 1, &w, z, op);
    memdraw::fill_poly(&mut want, &far, !0, &w, z, op);
    memdraw::ellipse(&mut want, p(12, 12), 9, 6, 0, &w, z, op);
    memdraw::fill_ellipse(&mut want, p(52, 36), 6, 8, &w, z, op);
    memdraw::arc(&mut want, p(32, 24), 14, 14, 1, &w, z, 200, -120, op);
    memdraw::fill_arc(&mut want, p(55, 8), 7, 7, &w, z, 0, 90, op);
    if got != want.unload(r).unwrap() {
        return Err(String::from("pixels differ from memdraw's"));
    }
    // A filled disc of radius 2 is 21 pixels.
    let disc = d
        .alloc(Rect::new(0, 0, 8, 8), GREY8, false, 0x000000FF)
        .map_err(e)?;
    d.fill_ellipse(&disc, p(4, 4), 2, 2, &white, z).map_err(e)?;
    let px = d.unload(&disc, disc.r).map_err(e)?;
    let n = px.iter().filter(|&&b| b == 255).count();
    if n != 21 {
        return Err(format!("disc of radius 2 has {n} pixels"));
    }
    Ok(())
}

/// Pixels written with `y` come back with `r`, on the screen too.
fn load_and_screen() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let screen = d.screen();
    let r = Rect::new(100, 50, 140, 70);
    let data: Vec<u8> = (0..r.dx() * r.dy() * 4)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    d.load(&screen, r, &data).map_err(e)?;
    d.flush().map_err(e)?;
    let got = d.unload(&screen, r).map_err(e)?;
    if got != data {
        return Err(String::from("screen pixels differ from those written"));
    }
    // Another connection sees the same screen.
    let mut d2 = Display::open().map_err(e)?;
    let s2 = d2.screen();
    if d2.unload(&s2, r).map_err(e)? != data {
        return Err(String::from("a second connection sees another screen"));
    }
    Ok(())
}

/// devdraw's error strings, for messages written straight to `data`.
fn errors() -> Result<(), String> {
    let d = Display::open().map_err(e)?;
    let mut data = OpenOptions::new()
        .write(true)
        .open(format!("/dev/draw/{}/data", d.conn))
        .map_err(|x| x.to_string())?;
    let mut bad_draw = vec![b'd'];
    bad_draw.extend([0u32, 77, 0].iter().flat_map(|v| v.to_le_bytes()));
    bad_draw.extend([0u8; 32]);
    let mut outside = vec![b'r', 0, 0, 0, 0];
    outside.extend([0i32, 0, 641, 1].iter().flat_map(|v| v.to_le_bytes()));
    for (msg, want) in [
        (&b"Q"[..], "bad draw command"),
        (&b"b\x01\x00"[..], "short draw message"),
        (&b"f\x09\x00\x00\x00"[..], "unknown id for draw image"),
        (&bad_draw[..], "unknown id for draw image"),
        (&outside[..], "readimage outside image"),
    ] {
        match data.write_all(msg) {
            Ok(_) => return Err(format!("{:?} was accepted", msg[0] as char)),
            Err(x) => {
                let got = Error::from(x).0;
                if got != want {
                    return Err(format!("{:?}: got {got:?}, want {want:?}", msg[0] as char));
                }
            }
        }
    }
    Ok(())
}

/// Draws `s` in the font file `path` onto `dst` with its line's top left
/// at `p`, glyph by glyph from the subfonts, without a cache; the rune
/// lookup is the SDK's (a range lacking the glyph passes it on, then
/// rune 0). Returns the x after the last glyph.
fn local_string(dst: &mut MemImage, p: Point, src: &MemImage, path: &str, s: &str) -> i32 {
    let font = FontFile::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    let dir = &path[..path.rfind('/').unwrap() + 1];
    let sub = |f: &str| Subfont::read(&std::fs::read(format!("{dir}{f}")).unwrap()).unwrap();
    let find = |c: char| {
        let r = u32::from(c);
        font.ranges.iter().find_map(|g| {
            if r < g.min || r > g.max {
                return None;
            }
            let sf = sub(&g.file);
            let i = (r - g.min + g.offset) as usize;
            sf.has(i).then_some((sf, i))
        })
    };
    let mut x = p.x;
    for c in s.chars() {
        let Some((sf, i)) = find(c).or_else(|| find('\0')) else {
            continue;
        };
        let (fc, next) = (sf.info[i], sf.info[i + 1]);
        let shift = font.ascent - sf.ascent;
        let r = Rect::new(
            x + i32::from(fc.left),
            p.y + i32::from(fc.top) + shift,
            x + i32::from(fc.left) + i32::from(next.x) - i32::from(fc.x),
            p.y + i32::from(fc.bottom) + shift,
        );
        let mp = Point::new(i32::from(fc.x), i32::from(fc.top));
        memdraw::draw(dst, r, src, r.min, Some(&sf.bits), mp, Op::SOVER_D);
        x += i32::from(fc.width);
    }
    x
}

const SANS: &str = "/fonts/sans/unicode.13.font";

/// Latin, Greek and Cyrillic from Go Regular; CJK, Hangul and a box
/// drawing from Unifont; a private-use rune neither has, which falls back
/// to rune 0.
const TEXT: &str = "Purgatorio: wörld αβγ Жж 中文 한 ⌘ \u{E000}!";

fn text() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let mut font = Font::open(SANS).map_err(e)?;
    let r = Rect::new(0, 0, 320, 40);
    let dst = d.alloc(r, GREY8, false, 0x000000FF).map_err(e)?;
    let white = d.color(0xFFFFFFFF).map_err(e)?;
    let p = Point::new(3, 4);
    let end = d
        .string(&dst, p, &white, Point::new(0, 0), &mut font, TEXT)
        .map_err(e)?;
    let got = d.unload(&dst, r).map_err(e)?;
    let mut want = local(r, GREY8, 0x000000FF);
    let x = local_string(&mut want, p, &solid(GREY8, 0xFFFFFFFF), SANS, TEXT);
    if end != Point::new(x, p.y) {
        return Err(format!("string ends at {end:?}, want ({x}, {})", p.y));
    }
    if font.string_width(TEXT) != x - p.x {
        return Err(format!("string_width is {}", font.string_width(TEXT)));
    }
    if got != want.unload(r).unwrap() {
        return Err(String::from("pixels differ from the subfonts' glyphs"));
    }
    if got.iter().filter(|&&b| b != 0).count() < 200 {
        return Err(String::from("hardly any ink"));
    }
    // With a background: the line's height from p to the end, then text.
    let grey = d.color(0x404040FF).map_err(e)?;
    let dst2 = d.alloc(r, GREY8, false, 0x000000FF).map_err(e)?;
    let z = Point::new(0, 0);
    d.string_bg(&dst2, p, &white, z, &mut font, TEXT, &grey, z)
        .map_err(e)?;
    let got = d.unload(&dst2, r).map_err(e)?;
    let mut want = local(r, GREY8, 0x000000FF);
    let bg = Rect::new(p.x, p.y, x, p.y + font.height);
    memdraw::draw(
        &mut want,
        bg,
        &solid(GREY8, 0x404040FF),
        z,
        None,
        z,
        Op::SOVER_D,
    );
    local_string(&mut want, p, &solid(GREY8, 0xFFFFFFFF), SANS, TEXT);
    if got != want.unload(r).unwrap() {
        return Err(String::from("stringbg pixels differ"));
    }
    font.free(&mut d).map_err(e)?;
    d.flush().map_err(e)
}

/// More distinct glyphs than the cache has slots, in a few lines, so that
/// slots are reused while strings are drawn.
fn cache_reuse() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let mut font = Font::open("/fonts/mono/unicode.13.font").map_err(e)?;
    let r = Rect::new(0, 0, 1200, 90);
    let dst = d.alloc(r, GREY8, false, 0x000000FF).map_err(e)?;
    let white = d.color(0xFFFFFFFF).map_err(e)?;
    let mut want = local(r, GREY8, 0x000000FF);
    let lw = solid(GREY8, 0xFFFFFFFF);
    let z = Point::new(0, 0);
    let n = NCACHE + 150;
    let runes: Vec<char> = (0..n as u32)
        .filter_map(|i| char::from_u32(0x4E00 + i))
        .collect();
    for (row, line) in runes.chunks(70).enumerate() {
        let line: String = line.iter().collect();
        // Every other line repeats an early one, from the cache or not.
        let p = Point::new(2, 2 + (row as i32 % 5) * 17);
        d.string(&dst, p, &white, z, &mut font, &line).map_err(e)?;
        local_string(&mut want, p, &lw, "/fonts/mono/unicode.13.font", &line);
    }
    let got = d.unload(&dst, r).map_err(e)?;
    if got != want.unload(r).unwrap() {
        return Err(String::from("pixels differ after the cache filled"));
    }
    font.free(&mut d).map_err(e)
}

/// devdraw's errors for the font cache messages.
fn font_errors() -> Result<(), String> {
    let mut d = Display::open().map_err(e)?;
    let img = d.alloc(Rect::new(0, 0, 8, 8), GREY8, false, 0).map_err(e)?;
    d.send().map_err(e)?;
    let mut data = OpenOptions::new()
        .write(true)
        .open(format!("/dev/draw/{}/data", d.conn))
        .map_err(|x| x.to_string())?;
    let id = img.id.to_le_bytes();
    let init = |id: [u8; 4], n: u32| {
        let mut m = vec![b'i'];
        m.extend(id);
        m.extend(n.to_le_bytes());
        m.push(6);
        m
    };
    let mut load = vec![b'l'];
    load.extend(id);
    load.extend(id);
    load.extend(9u16.to_le_bytes());
    load.extend([0u8; 26]);
    let mut string = vec![b's'];
    string.extend([0u8; 4]);
    string.extend(id);
    string.extend(id);
    string.extend([0u8; 32]);
    string.extend(1u16.to_le_bytes());
    string.extend(9u16.to_le_bytes());
    for (msg, want) in [
        (init([0; 4], 4), "cannot use display as font"),
        (init(id, 0), "bad font size (4096 chars max)"),
        (init(id, 4097), "bad font size (4096 chars max)"),
        (init(77u32.to_le_bytes(), 4), "unknown id for draw image"),
        (load.clone(), "image not a font"),
        (string.clone(), "image not a font"),
        (init(id, 4), ""),
        (load, "character index out of range"),
        (string, "character index out of range"),
        (b"i\x01".to_vec(), "short draw message"),
    ] {
        match (data.write_all(&msg), want) {
            (Ok(_), "") => {}
            (Ok(_), _) => return Err(format!("{:?} was accepted", msg[0] as char)),
            (Err(x), _) => {
                let got = Error::from(x).0;
                if got != want {
                    return Err(format!("{:?}: got {got:?}, want {want:?}", msg[0] as char));
                }
            }
        }
    }
    Ok(())
}

fn pointer() -> Result<(), String> {
    let mut m = Mouse::open().map_err(e)?;
    m.set(Point::new(5, 6)).map_err(e)?;
    let p = m.read().map_err(e)?;
    if p.xy != Point::new(5, 6) || p.buttons != 0 {
        return Err(format!("pointer read {p:?}"));
    }
    Ok(())
}

/// The pixel at `p` of what `d` reads of `i`, as `0xRRGGBB`.
fn pixel(d: &mut Display, i: &purgatorio::draw::Image, p: Point) -> Result<u32, String> {
    let px = d
        .unload(i, Rect::new(p.x, p.y, p.x + 1, p.y + 1))
        .map_err(e)?;
    Ok(u32::from_le_bytes([px[0], px[1], px[2], 0]))
}

/// Screens, windows and named images: one connection puts a public
/// screen on the display and a named window on it, as a window system
/// does; another looks the window up by name and draws in a window of
/// its own on it, as a program in that window does.
fn windows() -> Result<(), String> {
    let mut wm = Display::open().map_err(e)?;
    let screen = wm.screen();
    let grey = wm.color(0x777777FF).map_err(e)?;
    let s = wm.alloc_screen(&screen, &grey, true).map_err(e)?;
    let r = Rect::new(300, 200, 400, 260);
    let w = wm.alloc_window(&s, r, 0x55AAAAFF).map_err(e)?;
    wm.name_image(&w, "test.draw.window", true).map_err(e)?;
    let mut client = Display::open().map_err(e)?;
    let named = client.named("test.draw.window").map_err(e)?;
    if named.r != r {
        return Err(format!("the named window is {named:?}"));
    }
    let white = client.color(0xFFFFFFFF).map_err(e)?;
    let red = client.color(0xFF0000FF).map_err(e)?;
    let cs = client.alloc_screen(&named, &white, false).map_err(e)?;
    let inner = purgatorio::draw::inset(r, purgatorio::draw::BORDER);
    let cw = client.alloc_window(&cs, inner, 0xFFFFFFFF).map_err(e)?;
    client
        .draw(&cw, cw.r, &red, None, Point::new(0, 0))
        .map_err(e)?;
    client.flush().map_err(e)?;
    let at = |d: &mut Display, x, y| pixel(d, &screen, Point::new(x, y));
    let got = [at(&mut wm, 301, 201)?, at(&mut wm, 350, 230)?];
    if got != [0x55AAAA, 0xFF0000] {
        return Err(format!("border and inside are {got:06x?}"));
    }
    // A window on top covers it; drawing below shows only where it is
    // not covered.
    let over = wm
        .alloc_window(&s, Rect::new(350, 150, 450, 250), 0x0000FFFF)
        .map_err(e)?;
    client
        .draw(&cw, cw.r, &white, None, Point::new(0, 0))
        .map_err(e)?;
    client.flush().map_err(e)?;
    let got = [at(&mut wm, 320, 230)?, at(&mut wm, 380, 230)?];
    if got != [0xFFFFFF, 0x0000FF] {
        return Err(format!("under a window: {got:06x?}"));
    }
    wm.top_windows(&[&w], true).map_err(e)?;
    wm.flush().map_err(e)?;
    if at(&mut wm, 380, 230)? != 0xFFFFFF {
        return Err(String::from("the raised window lost its pixels"));
    }
    // Freed by the window system, the window leaves the screen, and the
    // screen's fill shows.
    wm.free(w).map_err(e)?;
    wm.free(over).map_err(e)?;
    wm.flush().map_err(e)?;
    if at(&mut wm, 350, 230)? != 0x777777 {
        return Err(String::from("the freed window stayed"));
    }
    Ok(())
}

/// Writes the screen to standard output (`-d`).
fn dump() -> Result<(), Error> {
    let mut d = Display::open()?;
    let screen = d.display_image()?;
    let px = d.unload(&screen, screen.r)?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{} {}", screen.r.dx(), screen.r.dy())?;
    out.write_all(&px)?;
    out.flush()?;
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).is_some_and(|a| a == "-d") {
        return dump();
    }
    let checks: &[(&str, Check)] = &[
        ("screen", screen_info),
        ("ctl", ctl_info),
        ("draw", draw_and_read_back),
        ("shapes", shapes),
        ("load", load_and_screen),
        ("errors", errors),
        ("text", text),
        ("cache", cache_reuse),
        ("fonterrors", font_errors),
        ("pointer", pointer),
        ("windows", windows),
    ];
    let mut failed = false;
    for (name, check) in checks {
        match check() {
            Ok(()) => println!("ok {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed = true;
            }
        }
    }
    if failed {
        return Err(Error::from("fail:draw"));
    }
    Ok(())
}
