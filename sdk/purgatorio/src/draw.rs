//! A small draw library over `/dev/draw` (draw(3)), in the spirit of
//! Inferno's Draw module: open the display, allocate images, draw
//! rectangles, lines, polygons and ellipses, load and read pixels, and
//! read the pointer and the keyboard.
//!
//! Drawing calls are buffered, as libdraw buffers them, and reach the
//! device on [`Display::flush`] (or when the buffer fills). An error in a
//! buffered message is reported by the call that sends it.
//!
//! ```ignore
//! let mut d = Display::open()?;
//! let red = d.color(0xFF0000FF)?;
//! let screen = d.screen();
//! d.draw(&screen, Rect::new(10, 10, 100, 100), &red, None, Point::new(0, 0));
//! d.flush()?;
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

pub use purg_memdraw::{
    ARGB32, Chan, ENDARROW, ENDDISC, ENDSQUARE, GREY1, GREY8, Op, Point, RGB24, RGBA32, Rect,
    XRGB32, bytes_per_line,
};

use crate::Error;

/// memdraw itself, for programs that draw into an image of their own
/// and send it with [`Display::load`].
pub use purg_memdraw as memdraw;

/// The most bytes buffered before a write, as libdraw keeps its writes
/// small enough for one Styx message.
const BUFSIZE: usize = 8000;

/// The fill colour for images that need none.
pub const TRANSPARENT: u32 = 0x0000_0000;
pub const BLACK: u32 = 0x0000_00FF;
pub const WHITE: u32 = 0xFFFF_FFFF;

/// An image on the draw device: an id and what the device was told about
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub id: u32,
    pub chan: Chan,
    pub repl: bool,
    pub r: Rect,
    pub clipr: Rect,
}

/// One connection to the draw device.
pub struct Display {
    /// Holds the connection open; `ctl`.
    ctl: File,
    data: File,
    /// The connection number: `/dev/draw/<n>`.
    pub conn: u32,
    screen: Image,
    buf: Vec<u8>,
    next: u32,
    opaque: Option<u32>,
}

fn field(info: &[u8], i: usize) -> Result<&str, Error> {
    let f = info
        .get(i * 12..i * 12 + 12)
        .ok_or_else(|| Error::from("short draw info"))?;
    std::str::from_utf8(f)
        .map(str::trim)
        .map_err(|_| Error::from("bad draw info"))
}

fn num(info: &[u8], i: usize) -> Result<i32, Error> {
    field(info, i)?
        .parse()
        .map_err(|_| Error::from("bad draw info"))
}

/// Parses the 144 bytes `new` and `ctl` read as: connection, image id,
/// channels, replication bit, rectangle, clip rectangle.
fn parse_info(info: &[u8]) -> Result<(u32, Image), Error> {
    let chan = Chan::parse(field(info, 2)?).ok_or_else(|| Error::from("bad draw info"))?;
    let rect = |i| -> Result<Rect, Error> {
        Ok(Rect::new(
            num(info, i)?,
            num(info, i + 1)?,
            num(info, i + 2)?,
            num(info, i + 3)?,
        ))
    };
    Ok((
        num(info, 0)? as u32,
        Image {
            id: num(info, 1)? as u32,
            chan,
            repl: num(info, 3)? != 0,
            r: rect(4)?,
            clipr: rect(8)?,
        },
    ))
}

fn put_rect(m: &mut Vec<u8>, r: Rect) {
    for v in [r.min.x, r.min.y, r.max.x, r.max.y] {
        m.extend(v.to_le_bytes());
    }
}

fn put_point(m: &mut Vec<u8>, p: Point) {
    m.extend(p.x.to_le_bytes());
    m.extend(p.y.to_le_bytes());
}

/// Appends a polygon coordinate as devdraw codes it: a 7-bit signed
/// difference from the previous one in a byte, or the value in three.
fn put_coord(m: &mut Vec<u8>, x: i32, old: i32) {
    let d = x.wrapping_sub(old);
    if (-0x40..0x40).contains(&d) {
        m.push((d & 0x7F) as u8);
    } else {
        m.push(0x80 | (x & 0x7F) as u8);
        m.push((x >> 7) as u8);
        m.push((x >> 15) as u8);
    }
}

impl Display {
    /// Opens a new connection to `/dev/draw`.
    pub fn open() -> Result<Display, Error> {
        let mut ctl = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/draw/new")?;
        let mut info = [0u8; 144];
        let n = ctl.read(&mut info)?;
        let (conn, screen) = parse_info(&info[..n])?;
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/draw/{conn}/data"))?;
        Ok(Display {
            ctl,
            data,
            conn,
            screen,
            buf: Vec::with_capacity(BUFSIZE),
            next: 1,
            opaque: None,
        })
    }

    /// The screen image, id 0.
    pub fn screen(&self) -> Image {
        self.screen.clone()
    }

    /// Asks the device about image `id` through `ctl`.
    pub fn info(&mut self, id: u32) -> Result<Image, Error> {
        self.ctl.write_all(&id.to_le_bytes())?;
        let mut info = [0u8; 144];
        let n = self.ctl.read(&mut info)?;
        Ok(parse_info(&info[..n])?.1)
    }

    /// Adds a message, writing out what is buffered first if it would not
    /// fit.
    fn msg(&mut self, m: &[u8]) -> Result<(), Error> {
        if !self.buf.is_empty() && self.buf.len() + m.len() > BUFSIZE {
            self.write_out()?;
        }
        self.buf.extend_from_slice(m);
        Ok(())
    }

    fn write_out(&mut self) -> Result<(), Error> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let r = self.data.write(&self.buf);
        self.buf.clear();
        r?;
        Ok(())
    }

    /// Sends what is buffered and asks the device to show it (`v`).
    pub fn flush(&mut self) -> Result<(), Error> {
        self.msg(b"v")?;
        self.write_out()
    }

    /// Sends what is buffered without a flush to the display.
    pub fn send(&mut self) -> Result<(), Error> {
        self.write_out()
    }

    /// Allocates an image (`b`) filled with colour `rgba`
    /// (`0xRRGGBBAA`, premultiplied). A replicated image's clip is the
    /// whole plane.
    pub fn alloc(&mut self, r: Rect, chan: Chan, repl: bool, rgba: u32) -> Result<Image, Error> {
        let id = self.next;
        self.next += 1;
        let clipr = if repl { Rect::infinite() } else { r };
        let mut m = vec![b'b'];
        m.extend(id.to_le_bytes());
        m.extend(0u32.to_le_bytes());
        m.push(0);
        m.extend(chan.0.to_le_bytes());
        m.push(repl as u8);
        put_rect(&mut m, r);
        put_rect(&mut m, clipr);
        m.extend(rgba.to_le_bytes());
        self.msg(&m)?;
        Ok(Image {
            id,
            chan,
            repl,
            r,
            clipr,
        })
    }

    /// A 1×1 replicated image of one colour, to draw with.
    pub fn color(&mut self, rgba: u32) -> Result<Image, Error> {
        let chan = if rgba & 0xFF == 0xFF { RGB24 } else { RGBA32 };
        self.alloc(Rect::new(0, 0, 1, 1), chan, true, rgba)
    }

    /// Frees an image (`f`).
    pub fn free(&mut self, i: Image) -> Result<(), Error> {
        let mut m = vec![b'f'];
        m.extend(i.id.to_le_bytes());
        self.msg(&m)
    }

    /// Sets an image's replication bit and clip (`c`).
    pub fn set_clip(&mut self, i: &mut Image, repl: bool, clipr: Rect) -> Result<(), Error> {
        let mut m = vec![b'c'];
        m.extend(i.id.to_le_bytes());
        m.push(repl as u8);
        put_rect(&mut m, clipr);
        i.repl = repl;
        i.clipr = clipr;
        self.msg(&m)
    }

    fn op(&mut self, op: Op) -> Result<(), Error> {
        if op != Op::SOVER_D {
            self.msg(&[b'O', op.0])?;
        }
        Ok(())
    }

    /// draw(2)'s `gendrawop`: `src` aligned with `sp` and `mask` with
    /// `mp` onto `r` of `dst`. No mask is an opaque one.
    #[allow(clippy::too_many_arguments)]
    pub fn gendraw_op(
        &mut self,
        dst: &Image,
        r: Rect,
        src: &Image,
        sp: Point,
        mask: Option<&Image>,
        mp: Point,
        op: Op,
    ) -> Result<(), Error> {
        let mask = match mask {
            Some(m) => m.id,
            None => self.opaque()?,
        };
        self.op(op)?;
        let mut m = vec![b'd'];
        for id in [dst.id, src.id, mask] {
            m.extend(id.to_le_bytes());
        }
        put_rect(&mut m, r);
        put_point(&mut m, sp);
        put_point(&mut m, mp);
        self.msg(&m)
    }

    /// draw(2)'s `draw`: source and mask both aligned with `p`.
    pub fn draw(
        &mut self,
        dst: &Image,
        r: Rect,
        src: &Image,
        mask: Option<&Image>,
        p: Point,
    ) -> Result<(), Error> {
        self.gendraw_op(dst, r, src, p, mask, p, Op::SOVER_D)
    }

    /// The id of an opaque mask, allocated on first use.
    fn opaque(&mut self) -> Result<u32, Error> {
        if let Some(id) = self.opaque {
            return Ok(id);
        }
        let i = self.alloc(Rect::new(0, 0, 1, 1), GREY1, true, WHITE)?;
        self.opaque = Some(i.id);
        Ok(i.id)
    }

    /// draw(2)'s `line`.
    #[allow(clippy::too_many_arguments)]
    pub fn line(
        &mut self,
        dst: &Image,
        p0: Point,
        p1: Point,
        end0: i32,
        end1: i32,
        thick: i32,
        src: &Image,
        sp: Point,
    ) -> Result<(), Error> {
        let mut m = vec![b'L'];
        m.extend(dst.id.to_le_bytes());
        put_point(&mut m, p0);
        put_point(&mut m, p1);
        m.extend(end0.to_le_bytes());
        m.extend(end1.to_le_bytes());
        m.extend(thick.to_le_bytes());
        m.extend(src.id.to_le_bytes());
        put_point(&mut m, sp);
        self.msg(&m)
    }

    #[allow(clippy::too_many_arguments)]
    fn polymsg(
        &mut self,
        c: u8,
        dst: &Image,
        pts: &[Point],
        e0: i32,
        e1: i32,
        thick: i32,
        src: &Image,
        sp: Point,
    ) -> Result<(), Error> {
        if pts.is_empty() || pts.len() > 0x8000 {
            return Err(Error::from("bad polygon"));
        }
        let mut m = vec![c];
        m.extend(dst.id.to_le_bytes());
        m.extend(((pts.len() - 1) as u16).to_le_bytes());
        m.extend(e0.to_le_bytes());
        m.extend(e1.to_le_bytes());
        m.extend(thick.to_le_bytes());
        m.extend(src.id.to_le_bytes());
        put_point(&mut m, sp);
        let (mut ox, mut oy) = (0, 0);
        for p in pts {
            put_coord(&mut m, p.x, ox);
            put_coord(&mut m, p.y, oy);
            (ox, oy) = (p.x, p.y);
        }
        self.msg(&m)
    }

    /// draw(2)'s `poly`: lines joining `pts`.
    #[allow(clippy::too_many_arguments)]
    pub fn poly(
        &mut self,
        dst: &Image,
        pts: &[Point],
        end0: i32,
        end1: i32,
        thick: i32,
        src: &Image,
        sp: Point,
    ) -> Result<(), Error> {
        self.polymsg(b'p', dst, pts, end0, end1, thick, src, sp)
    }

    /// draw(2)'s `fillpoly`; `wind` is `!0` (nonzero) or 1 (odd-even).
    pub fn fill_poly(
        &mut self,
        dst: &Image,
        pts: &[Point],
        wind: i32,
        src: &Image,
        sp: Point,
    ) -> Result<(), Error> {
        self.polymsg(b'P', dst, pts, wind, 0, 0, src, sp)
    }

    #[allow(clippy::too_many_arguments)]
    fn ellipsemsg(
        &mut self,
        c: u8,
        dst: &Image,
        centre: Point,
        a: i32,
        b: i32,
        thick: i32,
        src: &Image,
        sp: Point,
        arc: Option<(i32, i32)>,
    ) -> Result<(), Error> {
        let mut m = vec![c];
        m.extend(dst.id.to_le_bytes());
        m.extend(src.id.to_le_bytes());
        put_point(&mut m, centre);
        for v in [a, b, thick] {
            m.extend(v.to_le_bytes());
        }
        put_point(&mut m, sp);
        let (alpha, phi) = match arc {
            Some((alpha, phi)) => (alpha as u32 | 1 << 31, phi),
            None => (0, 0),
        };
        m.extend(alpha.to_le_bytes());
        m.extend(phi.to_le_bytes());
        self.msg(&m)
    }

    /// draw(2)'s `ellipse`.
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(
        &mut self,
        dst: &Image,
        c: Point,
        a: i32,
        b: i32,
        thick: i32,
        src: &Image,
        sp: Point,
    ) -> Result<(), Error> {
        self.ellipsemsg(b'e', dst, c, a, b, thick, src, sp, None)
    }

    /// draw(2)'s `fillellipse`.
    pub fn fill_ellipse(
        &mut self,
        dst: &Image,
        c: Point,
        a: i32,
        b: i32,
        src: &Image,
        sp: Point,
    ) -> Result<(), Error> {
        self.ellipsemsg(b'E', dst, c, a, b, 0, src, sp, None)
    }

    /// draw(2)'s `arc`: from `alpha` counterclockwise through `phi`
    /// degrees.
    #[allow(clippy::too_many_arguments)]
    pub fn arc(
        &mut self,
        dst: &Image,
        c: Point,
        a: i32,
        b: i32,
        thick: i32,
        src: &Image,
        sp: Point,
        alpha: i32,
        phi: i32,
    ) -> Result<(), Error> {
        self.ellipsemsg(b'e', dst, c, a, b, thick, src, sp, Some((alpha, phi)))
    }

    /// draw(2)'s `fillarc`.
    #[allow(clippy::too_many_arguments)]
    pub fn fill_arc(
        &mut self,
        dst: &Image,
        c: Point,
        a: i32,
        b: i32,
        src: &Image,
        sp: Point,
        alpha: i32,
        phi: i32,
    ) -> Result<(), Error> {
        self.ellipsemsg(b'E', dst, c, a, b, 0, src, sp, Some((alpha, phi)))
    }

    /// Writes the pixels of `r` (rows of [`bytes_per_line`] bytes) into
    /// `dst` (`y`), a few rows to a message.
    pub fn load(&mut self, dst: &Image, r: Rect, data: &[u8]) -> Result<(), Error> {
        let bpl = bytes_per_line(r, dst.chan.depth());
        if r.dx() <= 0 || r.dy() <= 0 || data.len() < bpl * r.dy() as usize {
            return Err(Error::from("short image data"));
        }
        let rows = ((BUFSIZE - 21) / bpl.max(1)).max(1) as i32;
        let mut y = r.min.y;
        while y < r.max.y {
            let n = rows.min(r.max.y - y);
            let sub = Rect::new(r.min.x, y, r.max.x, y + n);
            let off = (y - r.min.y) as usize * bpl;
            let mut m = vec![b'y'];
            m.extend(dst.id.to_le_bytes());
            put_rect(&mut m, sub);
            m.extend_from_slice(&data[off..off + n as usize * bpl]);
            self.msg(&m)?;
            y += n;
        }
        Ok(())
    }

    /// Reads the pixels of `r` of `src` (`r`): rows of
    /// [`bytes_per_line`] bytes.
    pub fn unload(&mut self, src: &Image, r: Rect) -> Result<Vec<u8>, Error> {
        let bpl = bytes_per_line(r, src.chan.depth());
        let rows = ((BUFSIZE - 21) / bpl.max(1)).max(1) as i32;
        let mut out = Vec::with_capacity(bpl * r.dy().max(0) as usize);
        let mut y = r.min.y;
        while y < r.max.y {
            let n = rows.min(r.max.y - y);
            let mut m = vec![b'r'];
            m.extend(src.id.to_le_bytes());
            put_rect(&mut m, Rect::new(r.min.x, y, r.max.x, y + n));
            self.msg(&m)?;
            self.write_out()?;
            let want = bpl * n as usize;
            let mut buf = vec![0u8; want];
            let got = self.data.read(&mut buf)?;
            if got != want {
                return Err(Error::from("short read of image data"));
            }
            out.extend_from_slice(&buf);
            y += n;
        }
        Ok(out)
    }
}

/// One pointer state from `/dev/pointer` (pointer(3)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    pub xy: Point,
    /// Button 1 is bit 0, 2 is bit 1, 3 is bit 2; the wheel is 8 and 16.
    pub buttons: u32,
    pub msec: u32,
}

/// `/dev/pointer`.
pub struct Mouse {
    f: File,
}

impl Mouse {
    pub fn open() -> Result<Mouse, Error> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/pointer")?;
        Ok(Mouse { f })
    }

    /// Waits for the pointer to move or a button to change.
    pub fn read(&mut self) -> Result<Pointer, Error> {
        let mut b = [0u8; 49];
        let n = self.f.read(&mut b)?;
        let s = std::str::from_utf8(&b[..n]).map_err(|_| Error::from("bad pointer data"))?;
        let mut f = s
            .strip_prefix('m')
            .ok_or_else(|| Error::from("bad pointer data"))?
            .split_ascii_whitespace()
            .map(|v| v.parse::<i64>().unwrap_or(0));
        let mut next = || f.next().unwrap_or(0);
        Ok(Pointer {
            xy: Point::new(next() as i32, next() as i32),
            buttons: next() as u32,
            msec: next() as u32,
        })
    }

    /// Moves the pointer.
    pub fn set(&mut self, p: Point) -> Result<(), Error> {
        self.f.write_all(format!("m{} {}", p.x, p.y).as_bytes())?;
        Ok(())
    }
}

/// `/dev/keyboard`: typed characters, one at a time.
pub struct Keyboard {
    f: File,
    pending: Vec<u8>,
}

impl Keyboard {
    pub fn open() -> Result<Keyboard, Error> {
        Ok(Keyboard {
            f: File::open("/dev/keyboard")?,
            pending: Vec::new(),
        })
    }

    /// Waits for a character.
    pub fn read(&mut self) -> Result<char, Error> {
        loop {
            if let Ok(s) = std::str::from_utf8(&self.pending)
                && let Some(c) = s.chars().next()
            {
                self.pending.drain(..c.len_utf8());
                return Ok(c);
            }
            let mut b = [0u8; 64];
            let n = self.f.read(&mut b)?;
            if n == 0 {
                return Err(Error::from("keyboard closed"));
            }
            self.pending.extend_from_slice(&b[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coords() {
        let mut m = Vec::new();
        put_coord(&mut m, 5, 0);
        put_coord(&mut m, -1, 0);
        put_coord(&mut m, 1000, 0);
        assert_eq!(
            m,
            [5, 0x7F, 0x80 | (1000 & 0x7F) as u8, (1000 >> 7) as u8, 0]
        );
    }

    #[test]
    fn info() {
        let s = format!(
            "{:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} ",
            3, 0, "x8r8g8b8", 0, 0, 0, 640, 480, 0, 0, 640, 480
        );
        let (conn, i) = parse_info(s.as_bytes()).unwrap();
        assert_eq!(conn, 3);
        assert_eq!(i.chan, XRGB32);
        assert_eq!(i.r, Rect::new(0, 0, 640, 480));
    }
}
