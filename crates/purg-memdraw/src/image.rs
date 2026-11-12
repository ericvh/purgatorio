//! Images: pixels in memory, laid out as image(6) lays them out.
//!
//! A row holds the bytes that the pixels `r.min.x` to `r.max.x` fall in,
//! counting pixel `x` from bit `x * depth` of an imaginary row that starts
//! at x = 0, so that the same pixel sits at the same place in a byte
//! whatever the rectangle. Pixels narrower than a byte fill it from its
//! most significant bit; wider ones are stored little-endian.

use alloc::vec;
use alloc::vec::Vec;

use crate::chan::{Chan, Layout};
use crate::geom::{Point, Rect};

/// Bytes a row of `r` takes at `depth` bits per pixel (draw(2)'s
/// `bytesperline`).
pub fn bytes_per_line(r: Rect, depth: u32) -> usize {
    let d = i64::from(depth);
    let start = (i64::from(r.min.x) * d).div_euclid(8);
    let end = (i64::from(r.max.x) * d + 7).div_euclid(8);
    (end - start).max(0) as usize
}

#[derive(Clone, Debug)]
pub struct Image {
    pub r: Rect,
    pub clipr: Rect,
    /// Whether the image tiles the plane (draw(2)'s `repl`).
    pub repl: bool,
    chan: Chan,
    layout: Layout,
    /// Bytes per row.
    width: usize,
    /// The byte, counted from x = 0, that each row starts at.
    base: i64,
    data: Vec<u8>,
}

/// Errors from loading pixels, with devdraw's strings.
pub const EWRITEOUTSIDE: &str = "writeimage outside image";
pub const EREADOUTSIDE: &str = "readimage outside image";
pub const ESHORT: &str = "short draw message";
pub const EBADCOMPRESS: &str = "bad compressed image data";

impl Image {
    /// A new image of `r` in `chan`, every pixel zero. Fails for a
    /// descriptor memdraw cannot draw with, a rectangle with negative
    /// size or beyond the coordinates a client can name, or more than
    /// [`crate::MAX_IMAGE_BYTES`].
    pub fn new(r: Rect, chan: Chan) -> Option<Image> {
        if !chan.supported() || r.dx() < 0 || r.dy() < 0 || !r.inside(&Rect::infinite()) {
            return None;
        }
        let layout = chan.layout()?;
        let width = bytes_per_line(r, layout.depth);
        let size = width.checked_mul(r.dy() as usize)?;
        if size > crate::MAX_IMAGE_BYTES {
            return None;
        }
        Some(Image {
            r,
            clipr: r,
            repl: false,
            chan,
            layout,
            width,
            base: (i64::from(r.min.x) * i64::from(layout.depth)).div_euclid(8),
            data: vec![0; size],
        })
    }

    pub fn chan(&self) -> Chan {
        self.chan
    }

    pub fn depth(&self) -> u32 {
        self.layout.depth
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The bytes of row `y`, which must be in `r`.
    pub fn row(&self, y: i32) -> &[u8] {
        let o = (y - self.r.min.y) as usize * self.width;
        &self.data[o..o + self.width]
    }

    /// Byte offset of pixel `p` (in `r`) and, for pixels narrower than a
    /// byte, the shift of its bits within that byte.
    fn locate(&self, p: Point) -> (usize, u32) {
        let d = i64::from(self.layout.depth);
        let bit = i64::from(p.x) * d;
        let byte = bit.div_euclid(8) - self.base;
        let o = (p.y - self.r.min.y) as usize * self.width + byte as usize;
        let shift = if d < 8 {
            (8 - d - bit.rem_euclid(8)) as u32
        } else {
            0
        };
        (o, shift)
    }

    /// The raw value of pixel `p`, which must be in `r`.
    pub fn raw(&self, p: Point) -> u32 {
        let (o, shift) = self.locate(p);
        match self.layout.depth {
            d @ (1 | 2 | 4) => (u32::from(self.data[o]) >> shift) & ((1 << d) - 1),
            8 => u32::from(self.data[o]),
            16 => u32::from(u16::from_le_bytes([self.data[o], self.data[o + 1]])),
            24 => u32::from_le_bytes([self.data[o], self.data[o + 1], self.data[o + 2], 0]),
            _ => u32::from_le_bytes([
                self.data[o],
                self.data[o + 1],
                self.data[o + 2],
                self.data[o + 3],
            ]),
        }
    }

    /// Sets pixel `p`, which must be in `r`, to raw value `v`.
    pub fn set_raw(&mut self, p: Point, v: u32) {
        let (o, shift) = self.locate(p);
        let b = v.to_le_bytes();
        match self.layout.depth {
            d @ (1 | 2 | 4) => {
                let m = (((1u32 << d) - 1) << shift) as u8;
                self.data[o] = (self.data[o] & !m) | (((v << shift) as u8) & m);
            }
            8 => self.data[o] = b[0],
            16 => self.data[o..o + 2].copy_from_slice(&b[..2]),
            24 => self.data[o..o + 3].copy_from_slice(&b[..3]),
            _ => self.data[o..o + 4].copy_from_slice(&b),
        }
    }

    /// Pixel `p` as premultiplied `[r, g, b, a]`.
    pub fn rgba(&self, p: Point) -> [u8; 4] {
        self.layout.unpack(self.raw(p))
    }

    /// The raw value of a premultiplied colour in this image's channels.
    pub fn pack(&self, c: [u8; 4]) -> u32 {
        self.layout.pack(c)
    }

    /// Sets every pixel of `r ∩ self.r` to raw value `v`.
    pub fn fill_raw(&mut self, r: Rect, v: u32) {
        let Some(r) = r.clip(&self.r) else {
            return;
        };
        let d = self.layout.depth;
        if d < 8 {
            for y in r.min.y..r.max.y {
                for x in r.min.x..r.max.x {
                    self.set_raw(Point::new(x, y), v);
                }
            }
            return;
        }
        let bpp = (d / 8) as usize;
        let px = v.to_le_bytes();
        let (first, _) = self.locate(r.min);
        let mut run = Vec::with_capacity(r.dx() as usize * bpp);
        for _ in 0..r.dx() {
            run.extend_from_slice(&px[..bpp]);
        }
        for i in 0..r.dy() as usize {
            let o = first + i * self.width;
            self.data[o..o + run.len()].copy_from_slice(&run);
        }
    }

    /// Fills the whole image with a premultiplied colour, as `b`'s
    /// colour argument does.
    pub fn fill(&mut self, c: [u8; 4]) {
        let v = self.pack(c);
        self.fill_raw(self.r, v);
    }

    /// Copies row bytes of `r` from `src`, which has the same depth.
    /// `r` must be inside both images and, for pixels narrower than a
    /// byte, start and end on byte boundaries.
    pub(crate) fn copy_rows(&mut self, r: Rect, src: &Image, sp: Point) {
        let n = bytes_per_line(r, self.layout.depth);
        for i in 0..r.dy() {
            let (d, _) = self.locate(Point::new(r.min.x, r.min.y + i));
            let (s, _) = src.locate(Point::new(sp.x, sp.y + i));
            self.data[d..d + n].copy_from_slice(&src.data[s..s + n]);
        }
    }

    /// Writes the pixels of `r` from `data`, rows of
    /// [`bytes_per_line`] bytes as image(6) and the `y` message carry
    /// them. Returns the bytes used.
    pub fn load(&mut self, r: Rect, data: &[u8]) -> Result<usize, &'static str> {
        if r.is_empty() || !r.inside(&self.r) {
            return Err(EWRITEOUTSIDE);
        }
        let d = self.layout.depth;
        let bpl = bytes_per_line(r, d);
        let n = bpl * r.dy() as usize;
        if data.len() < n {
            return Err(ESHORT);
        }
        let aligned = d >= 8
            || (i64::from(r.min.x) * i64::from(d) % 8 == 0
                && i64::from(r.max.x) * i64::from(d) % 8 == 0);
        for (i, row) in data[..n].chunks(bpl).enumerate() {
            let y = r.min.y + i as i32;
            if aligned {
                let (o, _) = self.locate(Point::new(r.min.x, y));
                self.data[o..o + bpl].copy_from_slice(row);
                continue;
            }
            // Pixels narrower than a byte at a ragged edge: only those
            // inside r change.
            let base = (i64::from(r.min.x) * i64::from(d)).div_euclid(8);
            for x in r.min.x..r.max.x {
                let bit = i64::from(x) * i64::from(d);
                let byte = row[(bit.div_euclid(8) - base) as usize];
                let shift = 8 - i64::from(d) - bit.rem_euclid(8);
                let v = (u32::from(byte) >> shift) & ((1 << d) - 1);
                self.set_raw(Point::new(x, y), v);
            }
        }
        Ok(n)
    }

    /// Writes the pixels of `r` from compressed `data` (image(6)): a run
    /// of codes that each copy literal bytes or repeat earlier output.
    /// Returns the bytes used.
    pub fn load_compressed(&mut self, r: Rect, data: &[u8]) -> Result<usize, &'static str> {
        if r.is_empty() || !r.inside(&self.r) {
            return Err(EWRITEOUTSIDE);
        }
        let n = bytes_per_line(r, self.layout.depth) * r.dy() as usize;
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let mut i = 0;
        while out.len() < n {
            let c = *data.get(i).ok_or(ESHORT)?;
            i += 1;
            if c & 0x80 != 0 {
                let k = usize::from(c & 0x7F) + 1;
                let lit = data.get(i..i + k).ok_or(ESHORT)?;
                if out.len() + k > n {
                    return Err(EBADCOMPRESS);
                }
                out.extend_from_slice(lit);
                i += k;
            } else {
                let lo = *data.get(i).ok_or(ESHORT)?;
                i += 1;
                let k = usize::from(c >> 2) + 3;
                let back = (usize::from(c & 3) << 8 | usize::from(lo)) + 1;
                if back > out.len() || out.len() + k > n {
                    return Err(EBADCOMPRESS);
                }
                let from = out.len() - back;
                for j in 0..k {
                    let b = out[from + j];
                    out.push(b);
                }
            }
        }
        self.load(r, &out)?;
        Ok(i)
    }

    /// The pixels of `r` as rows of [`bytes_per_line`] bytes, as the `r`
    /// message returns them.
    pub fn unload(&self, r: Rect) -> Result<Vec<u8>, &'static str> {
        if r.is_empty() || !r.inside(&self.r) {
            return Err(EREADOUTSIDE);
        }
        let bpl = bytes_per_line(r, self.layout.depth);
        let mut out = Vec::with_capacity(bpl * r.dy() as usize);
        for y in r.min.y..r.max.y {
            let (o, _) = self.locate(Point::new(r.min.x, y));
            out.extend_from_slice(&self.data[o..o + bpl]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chan::{GREY1, GREY2, RGB24, XRGB32};

    #[test]
    fn bytes_per_line_follows_absolute_x() {
        assert_eq!(bytes_per_line(Rect::new(0, 0, 8, 1), 1), 1);
        assert_eq!(bytes_per_line(Rect::new(7, 0, 9, 1), 1), 2);
        assert_eq!(bytes_per_line(Rect::new(-1, 0, 1, 1), 1), 2);
        assert_eq!(bytes_per_line(Rect::new(3, 0, 5, 1), 24), 6);
    }

    #[test]
    fn pixels_round_trip() {
        let mut i = Image::new(Rect::new(-3, -2, 13, 5), GREY2).unwrap();
        for y in -2..5 {
            for x in -3..13 {
                i.set_raw(Point::new(x, y), ((x + y) & 3) as u32);
            }
        }
        for y in -2..5 {
            for x in -3..13 {
                assert_eq!(i.raw(Point::new(x, y)), ((x + y) & 3) as u32);
            }
        }
    }

    #[test]
    fn load_and_unload() {
        let r = Rect::new(1, 1, 4, 3);
        let mut i = Image::new(Rect::new(0, 0, 5, 4), RGB24).unwrap();
        let data: Vec<u8> = (0..18).collect();
        assert_eq!(i.load(r, &data), Ok(18));
        assert_eq!(i.unload(r).unwrap(), data);
        // Little-endian: the first byte is blue.
        assert_eq!(i.rgba(Point::new(1, 1)), [2, 1, 0, 255]);
        assert_eq!(i.load(Rect::new(0, 0, 6, 1), &data), Err(EWRITEOUTSIDE));
        assert_eq!(i.load(r, &data[..17]), Err(ESHORT));
    }

    #[test]
    fn ragged_one_bit_load_keeps_neighbours() {
        let mut i = Image::new(Rect::new(0, 0, 16, 1), GREY1).unwrap();
        i.fill_raw(i.r, 1);
        i.load(Rect::new(3, 0, 6, 1), &[0x00]).unwrap();
        assert_eq!(i.row(0), &[0b1110_0011, 0xFF]);
    }

    #[test]
    fn compressed_load() {
        let mut i = Image::new(Rect::new(0, 0, 2, 2), XRGB32).unwrap();
        // Four literal bytes, then a copy of 12 bytes from 4 back.
        let data = [0x83, 1, 2, 3, 4, (9 << 2), 3];
        assert_eq!(i.load_compressed(i.r, &data), Ok(7));
        assert_eq!(i.unload(i.r).unwrap(), [1, 2, 3, 4].repeat(4));
        assert_eq!(i.load_compressed(i.r, &data[..5]), Err(ESHORT));
    }
}
