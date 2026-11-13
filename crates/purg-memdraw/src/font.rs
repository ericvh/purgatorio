//! Fonts as font(6) describes them, and the glyph drawing of draw(3)'s
//! font cache.
//!
//! A subfont file is an image file (image(6)) holding the glyphs side by
//! side, then three 12-byte fields (the glyph count `n`, the height and
//! the ascent), then `n + 1` six-byte entries ([`Fontchar`]): `x` as two
//! bytes, low byte first, then `top`, `bottom`, `left` (signed) and
//! `width`. Glyph `i` is the rectangle from `(x[i], top[i])` to
//! `(x[i+1], bottom[i])` of the image; drawn at a point it lands `left`
//! pixels to the right, and the point then moves `width` pixels on. A
//! glyph with no ink and no width is missing. The last entry only closes
//! the glyph before it. A non-zero pixel is ink: the glyphs are a mask.
//!
//! A font file is text: a line with the height and the ascent, then lines
//! of two numbers, an optional third and a file name: the runes from the
//! first number to the second, inclusive, are the glyphs of that subfont
//! from the third number on (0 by default). Numbers are decimal, octal
//! with a leading 0, or hexadecimal with 0x, and names that do not start
//! with `/` are relative to the font file's directory.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::draw::{Op, draw};
use crate::file::{self, EBADIMAGE, ESHORTIMAGE, field, number};
use crate::geom::{Point, Rect};
use crate::image::Image;

/// One subfont entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fontchar {
    pub x: u16,
    pub top: u8,
    pub bottom: u8,
    pub left: i8,
    pub width: u8,
}

/// A subfont: glyph images and their entries.
#[derive(Clone, Debug)]
pub struct Subfont {
    pub height: i32,
    pub ascent: i32,
    /// `n + 1` entries for `n` glyphs.
    pub info: Vec<Fontchar>,
    pub bits: Image,
}

impl Subfont {
    /// The number of glyphs.
    pub fn n(&self) -> usize {
        self.info.len() - 1
    }

    /// Whether glyph `i` exists: it has ink or advances.
    pub fn has(&self, i: usize) -> bool {
        i < self.n() && (self.info[i].width != 0 || self.info[i + 1].x != self.info[i].x)
    }

    /// Reads the header and entries that follow a subfont's image, from
    /// the start of `a`: the height, the ascent and the entries.
    pub fn read_info(a: &[u8]) -> Result<(i32, i32, Vec<Fontchar>), &'static str> {
        let n = usize::try_from(number(a, 0)?).map_err(|_| EBADIMAGE)?;
        let (height, ascent) = (number(a, 12)?, number(a, 24)?);
        let e = a.get(36..36 + 6 * (n + 1)).ok_or(ESHORTIMAGE)?;
        let info = e
            .as_chunks::<6>()
            .0
            .iter()
            .map(|c| Fontchar {
                x: u16::from_le_bytes([c[0], c[1]]),
                top: c[2],
                bottom: c[3],
                left: c[4] as i8,
                width: c[5],
            })
            .collect();
        Ok((height, ascent, info))
    }

    /// Reads a subfont file.
    pub fn read(a: &[u8]) -> Result<Subfont, &'static str> {
        let (bits, o) = file::read_image(a)?;
        let (height, ascent, info) = Subfont::read_info(&a[o..])?;
        Ok(Subfont {
            height,
            ascent,
            info,
            bits,
        })
    }

    /// The subfont as a file, its image compressed or not.
    pub fn write(&self, compressed: bool) -> Vec<u8> {
        let mut out = file::write_image(&self.bits, compressed);
        for v in [self.n() as i32, self.height, self.ascent] {
            out.extend_from_slice(field(&format!("{v}")).as_bytes());
        }
        for c in &self.info {
            out.extend(c.x.to_le_bytes());
            out.extend([c.top, c.bottom, c.left as u8, c.width]);
        }
        out
    }
}

/// One range line of a font file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub min: u32,
    pub max: u32,
    /// The glyph of `min` in the subfont.
    pub offset: u32,
    pub file: String,
}

/// A font file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFile {
    pub height: i32,
    pub ascent: i32,
    pub ranges: Vec<Range>,
}

/// A number as C's `strtol` with base 0 reads it.
fn cnum(s: &str) -> Option<u32> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else if s.len() > 1
        && let Some(o) = s.strip_prefix('0')
    {
        u32::from_str_radix(o, 8).ok()
    } else {
        s.parse().ok()
    }
}

/// The error for a font file that does not parse.
pub const EBADFONT: &str = "bad font file";

impl FontFile {
    /// Parses a font file.
    pub fn parse(s: &str) -> Result<FontFile, &'static str> {
        let mut w = s.split_ascii_whitespace();
        let mut next = || w.next().ok_or(EBADFONT);
        let height = cnum(next()?).ok_or(EBADFONT)? as i32;
        let ascent = cnum(next()?).ok_or(EBADFONT)? as i32;
        if height <= 0 || ascent < 0 || ascent > height {
            return Err(EBADFONT);
        }
        let mut ranges = Vec::new();
        let mut w = s.split_ascii_whitespace().skip(2).peekable();
        while let Some(t) = w.next() {
            let min = cnum(t).ok_or(EBADFONT)?;
            let max = cnum(w.next().ok_or(EBADFONT)?).ok_or(EBADFONT)?;
            let t = w.next().ok_or(EBADFONT)?;
            let (offset, file) = match cnum(t) {
                Some(o) => (o, w.next().ok_or(EBADFONT)?),
                None => (0, t),
            };
            if max < min {
                return Err(EBADFONT);
            }
            ranges.push(Range {
                min,
                max,
                offset,
                file: String::from(file),
            });
        }
        Ok(FontFile {
            height,
            ascent,
            ranges,
        })
    }

    /// The font file as text: ranges in hexadecimal, an offset only where
    /// it is not 0.
    pub fn format(&self) -> String {
        let mut s = format!("{} {}\n", self.height, self.ascent);
        for r in &self.ranges {
            if r.offset == 0 {
                s.push_str(&format!("0x{:04X} 0x{:04X} {}\n", r.min, r.max, r.file));
            } else {
                s.push_str(&format!(
                    "0x{:04X} 0x{:04X} {} {}\n",
                    r.min, r.max, r.offset, r.file
                ));
            }
        }
        s
    }
}

/// A glyph in a font cache image, as the `l` message records it: the
/// rectangle it occupies in the cache, its offset and its advance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheChar {
    pub minx: i32,
    pub maxx: i32,
    pub miny: u8,
    pub maxy: u8,
    pub left: i8,
    pub width: u8,
}

/// Draws glyph `c` of the font cache `cache` (whose ascent is `ascent`)
/// with its baseline origin at `p`: `src`, aligned so that `*sp` falls on
/// `p`, through the glyph as a mask, as devdraw's `drawchar` does. Moves
/// `p` and `*sp` on by the glyph's advance; returns the new point and
/// what changed.
#[allow(clippy::too_many_arguments)]
pub fn draw_char(
    dst: &mut Image,
    p: Point,
    src: &Image,
    sp: &mut Point,
    cache: &Image,
    ascent: i32,
    c: &CacheChar,
    op: Op,
) -> (Point, Option<Rect>) {
    let x = p.x + i32::from(c.left);
    let y = p.y - (ascent - i32::from(c.miny));
    let r = Rect::new(
        x,
        y,
        x + (c.maxx - c.minx),
        y + (i32::from(c.maxy) - i32::from(c.miny)),
    );
    let sp1 = Point::new(sp.x + i32::from(c.left), sp.y + i32::from(c.miny));
    let damage = draw(
        dst,
        r,
        src,
        sp1,
        Some(cache),
        Point::new(c.minx, i32::from(c.miny)),
        op,
    );
    sp.x += i32::from(c.width);
    (Point::new(p.x + i32::from(c.width), p.y), damage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chan::GREY1;

    #[test]
    fn font_files() {
        let f =
            FontFile::parse("16\t13\n0 0x7F ascii.13\n0x0100 0x017F 0x10 latin.13\n0400 0401 /x\n")
                .unwrap();
        assert_eq!((f.height, f.ascent), (16, 13));
        assert_eq!(f.ranges.len(), 3);
        assert_eq!(f.ranges[1].offset, 16);
        assert_eq!(f.ranges[1].file, "latin.13");
        assert_eq!(f.ranges[2].min, 0o400);
        assert_eq!(FontFile::parse(&f.format()).unwrap(), f);
        assert_eq!(FontFile::parse("16"), Err(EBADFONT));
        assert_eq!(FontFile::parse("16 13\n5 4 x\n"), Err(EBADFONT));
    }

    #[test]
    fn subfonts_round_trip() {
        let mut bits = Image::new(Rect::new(0, 0, 10, 4), GREY1).unwrap();
        bits.fill_raw(Rect::new(1, 1, 3, 3), 1);
        let s = Subfont {
            height: 4,
            ascent: 3,
            info: alloc::vec![
                Fontchar {
                    x: 0,
                    top: 0,
                    bottom: 4,
                    left: -1,
                    width: 4
                },
                Fontchar {
                    x: 4,
                    top: 1,
                    bottom: 3,
                    left: 0,
                    width: 0
                },
                Fontchar {
                    x: 4,
                    top: 0,
                    bottom: 0,
                    left: 0,
                    width: 0
                },
            ],
            bits,
        };
        for compressed in [false, true] {
            let f = s.write(compressed);
            let t = Subfont::read(&f).unwrap();
            assert_eq!(t.info, s.info);
            assert_eq!((t.height, t.ascent, t.n()), (4, 3, 2));
            assert_eq!(t.bits.unload(t.bits.r), s.bits.unload(s.bits.r));
            assert!(t.has(0));
            assert!(!t.has(1));
        }
    }
}
