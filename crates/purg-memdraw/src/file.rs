//! image(6) files: reading and writing images in the external format,
//! plain or compressed.
//!
//! A plain file is five 12-byte fields, each a right-justified string and
//! a blank: the channels and the rectangle (`min.x min.y max.x max.y`),
//! then the rows as [`Image::unload`] gives them. A compressed file starts
//! with the 11 bytes `compressed\n` and the same header, followed by
//! blocks: two 12-byte fields (the y one past the block's last row, and
//! the count of data bytes), then the block's rows coded as
//! [`Image::load_compressed`] reads them. A block holds whole rows, and
//! its copies reach back only within the block.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::chan::{Chan, GREY1, GREY2, GREY4, GREY8};
use crate::geom::Rect;
use crate::image::{Image, bytes_per_line};

/// Errors from reading an image file.
pub const EBADIMAGE: &str = "bad image file";
pub const ESHORTIMAGE: &str = "short image file";

/// The smallest block size a writer uses; image(6) readers accept blocks
/// of at least this many bytes, and of twice a row when that is larger.
pub const NCBLOCK: usize = 6000;

/// The block size for rows of `r` at `depth`, as Inferno's writer picks
/// it: [`NCBLOCK`], or two rows if that is more.
pub fn block_size(r: Rect, depth: u32) -> usize {
    (2 * bytes_per_line(r, depth)).max(NCBLOCK)
}

/// A 12-byte field: the value right-justified in 11 bytes and a blank.
pub fn field(s: &str) -> String {
    format!("{s:>11} ")
}

fn header(chan: Chan, r: Rect) -> String {
    let mut s = field(&format!("{chan}"));
    for v in [r.min.x, r.min.y, r.max.x, r.max.y] {
        s.push_str(&field(&format!("{v}")));
    }
    s
}

/// Parses the 12-byte field at `a[o..]` as text.
fn text(a: &[u8], o: usize) -> Result<&str, &'static str> {
    let f = a.get(o..o + 12).ok_or(ESHORTIMAGE)?;
    core::str::from_utf8(f)
        .map(str::trim)
        .map_err(|_| EBADIMAGE)
}

/// Parses the 12-byte decimal field at `a[o..]`.
pub fn number(a: &[u8], o: usize) -> Result<i32, &'static str> {
    text(a, o)?.parse().map_err(|_| EBADIMAGE)
}

/// The channels of a header field: a descriptor, or one of the old
/// log-depth digits `0` to `3` for 1-, 2-, 4- and 8-bit grey.
fn chan_field(s: &str) -> Option<Chan> {
    match s {
        "0" => Some(GREY1),
        "1" => Some(GREY2),
        "2" => Some(GREY4),
        "3" => Some(GREY8),
        _ => Chan::parse(s),
    }
}

/// The header of an image file at the start of `a`: whether it is
/// compressed, its channels, its rectangle, and the header's length.
pub fn read_header(a: &[u8]) -> Result<(bool, Chan, Rect, usize), &'static str> {
    let compressed = a.starts_with(b"compressed\n");
    let o = if compressed { 11 } else { 0 };
    let chan = chan_field(text(a, o)?).ok_or(EBADIMAGE)?;
    let r = Rect::new(
        number(a, o + 12)?,
        number(a, o + 24)?,
        number(a, o + 36)?,
        number(a, o + 48)?,
    );
    if r.dx() < 0 || r.dy() < 0 {
        return Err(EBADIMAGE);
    }
    Ok((compressed, chan, r, o + 60))
}

/// One block of a compressed image: the rows it covers and its data.
pub struct Block<'a> {
    pub r: Rect,
    pub data: &'a [u8],
}

/// The blocks of a compressed image of rectangle `r` whose blocks start at
/// `a[0]`, and the bytes they take.
pub fn blocks(a: &[u8], r: Rect) -> Result<(Vec<Block<'_>>, usize), &'static str> {
    let mut out = Vec::new();
    let (mut o, mut y) = (0, r.min.y);
    while y < r.max.y {
        let maxy = number(a, o)?;
        let n = usize::try_from(number(a, o + 12)?).map_err(|_| EBADIMAGE)?;
        if maxy <= y || maxy > r.max.y {
            return Err(EBADIMAGE);
        }
        let data = a.get(o + 24..o + 24 + n).ok_or(ESHORTIMAGE)?;
        out.push(Block {
            r: Rect::new(r.min.x, y, r.max.x, maxy),
            data,
        });
        o += 24 + n;
        y = maxy;
    }
    Ok((out, o))
}

/// Reads the image file at the start of `a`; returns the image and the
/// bytes it took.
pub fn read_image(a: &[u8]) -> Result<(Image, usize), &'static str> {
    let (compressed, chan, r, mut o) = read_header(a)?;
    let mut i = Image::new(r, chan).ok_or(EBADIMAGE)?;
    if r.is_empty() {
        return Ok((i, o));
    }
    if compressed {
        let (bl, n) = blocks(&a[o..], r)?;
        for b in bl {
            let used = i.load_compressed(b.r, b.data)?;
            if used != b.data.len() {
                return Err(EBADIMAGE);
            }
        }
        o += n;
    } else {
        let n = bytes_per_line(r, chan.depth()) * r.dy() as usize;
        let data = a.get(o..o + n).ok_or(ESHORTIMAGE)?;
        i.load(r, data)?;
        o += n;
    }
    Ok((i, o))
}

/// Writes `i` (its whole rectangle) as an image file, compressed or not.
pub fn write_image(i: &Image, compressed: bool) -> Vec<u8> {
    let r = i.r;
    let mut out = Vec::new();
    if compressed {
        out.extend_from_slice(b"compressed\n");
    }
    out.extend_from_slice(header(i.chan(), r).as_bytes());
    if r.is_empty() {
        return out;
    }
    let rows = i.unload(r).expect("an image's own rectangle");
    if !compressed {
        out.extend(rows);
        return out;
    }
    let bpl = bytes_per_line(r, i.depth());
    let max = block_size(r, i.depth());
    let mut y = r.min.y;
    while y < r.max.y {
        let start = (y - r.min.y) as usize * bpl;
        let (data, n) = compress_rows(&rows[start..], bpl, max);
        y += n as i32;
        out.extend_from_slice(field(&format!("{y}")).as_bytes());
        out.extend_from_slice(field(&format!("{}", data.len())).as_bytes());
        out.extend(data);
    }
    out
}

/// The longest copy and the farthest it reaches back.
const MAXRUN: usize = 34;
const MAXOFF: usize = 1024;
/// The longest literal run.
const MAXLIT: usize = 128;
const HASH: usize = 1 << 12;
/// The most earlier positions tried for each match.
const CHAIN: usize = 64;

fn hash(b: &[u8]) -> usize {
    (usize::from(b[0]) << 4 ^ usize::from(b[1]) << 2 ^ usize::from(b[2])) & (HASH - 1)
}

/// Compresses as many whole rows of `rows` (each `bpl` bytes) as fit in a
/// block of `max` bytes, at least one; returns the block's data and the
/// rows it holds. Copies stay within the block and do not cross the end
/// of the row they are in, so a row's code does not depend on the rows
/// after it.
fn compress_rows(rows: &[u8], bpl: usize, max: usize) -> (Vec<u8>, usize) {
    let nrows = rows.len() / bpl;
    let mut out = Vec::new();
    let mut head = vec![usize::MAX; HASH];
    let mut prev: Vec<usize> = Vec::new();
    let mut done = 0;
    while done < nrows {
        let (rs, re) = (done * bpl, (done + 1) * bpl);
        let before = out.len();
        prev.resize(re, usize::MAX);
        let insert = |p: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
            if p + 3 <= re {
                let h = hash(&rows[p..]);
                prev[p] = head[h];
                head[h] = p;
            }
        };
        let (mut i, mut lit) = (rs, rs);
        while i < re {
            let (mut best, mut off) = (0, 0);
            if i + 3 <= re {
                let mut c = head[hash(&rows[i..])];
                let limit = MAXRUN.min(re - i);
                let mut steps = 0;
                while c != usize::MAX && i - c <= MAXOFF && steps < CHAIN {
                    let mut l = 0;
                    while l < limit && rows[c + l] == rows[i + l] {
                        l += 1;
                    }
                    if l > best {
                        (best, off) = (l, i - c);
                        if l == limit {
                            break;
                        }
                    }
                    c = prev[c];
                    steps += 1;
                }
            }
            if best >= 3 {
                literals(&mut out, &rows[lit..i]);
                out.push(((best - 3) << 2 | (off - 1) >> 8) as u8);
                out.push((off - 1) as u8);
                for p in i..i + best {
                    insert(p, &mut head, &mut prev);
                }
                i += best;
                lit = i;
            } else {
                insert(i, &mut head, &mut prev);
                i += 1;
            }
        }
        literals(&mut out, &rows[lit..re]);
        if out.len() > max && done > 0 {
            out.truncate(before);
            break;
        }
        done += 1;
    }
    (out, done)
}

fn literals(out: &mut Vec<u8>, mut b: &[u8]) {
    while !b.is_empty() {
        let n = b.len().min(MAXLIT);
        out.push(0x80 | (n - 1) as u8);
        out.extend_from_slice(&b[..n]);
        b = &b[n..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chan::{GREY1, RGB24};
    use crate::geom::Point;

    fn pattern(r: Rect, chan: Chan) -> Image {
        let mut i = Image::new(r, chan).unwrap();
        let mask = if chan.depth() >= 32 {
            u32::MAX
        } else {
            (1u32 << chan.depth()) - 1
        };
        for y in r.min.y..r.max.y {
            for x in r.min.x..r.max.x {
                let v = (x / 3 * 7 + y * 13 + (x * y) % 5) as u32;
                i.set_raw(Point::new(x, y), v & mask);
            }
        }
        i
    }

    #[test]
    fn round_trips() {
        for (r, chan) in [
            (Rect::new(0, 0, 37, 11), GREY8),
            (Rect::new(-5, 3, 70, 9), GREY1),
            (Rect::new(2, 2, 900, 30), RGB24),
            (Rect::new(0, 0, 0, 0), GREY8),
        ] {
            let i = pattern(r, chan);
            for compressed in [false, true] {
                let f = write_image(&i, compressed);
                let (j, n) = read_image(&f).unwrap();
                assert_eq!(n, f.len());
                assert_eq!(j.r, r);
                assert_eq!(j.chan(), chan);
                if !r.is_empty() {
                    assert_eq!(j.unload(r).unwrap(), i.unload(r).unwrap());
                }
            }
        }
    }

    #[test]
    fn blocks_hold_whole_rows_within_their_size() {
        let r = Rect::new(0, 0, 900, 30);
        let i = pattern(r, RGB24);
        let f = write_image(&i, true);
        let (_, _, _, o) = read_header(&f).unwrap();
        let (bl, _) = blocks(&f[o..], r).unwrap();
        assert!(bl.len() > 1);
        for b in &bl {
            assert!(b.data.len() <= block_size(r, 24));
        }
    }

    #[test]
    fn a_blank_image_compresses() {
        let i = Image::new(Rect::new(0, 0, 256, 16), GREY8).unwrap();
        let f = write_image(&i, true);
        assert!(f.len() < 400, "{}", f.len());
        assert_eq!(
            read_image(&f).unwrap().0.unload(i.r).unwrap(),
            vec![0; 4096]
        );
    }

    #[test]
    fn old_depth_headers() {
        let mut f = header(GREY1, Rect::new(0, 0, 8, 1)).into_bytes();
        f[..12].copy_from_slice(b"          0 ");
        f.push(0xA5);
        let (i, _) = read_image(&f).unwrap();
        assert_eq!(i.chan(), GREY1);
        assert_eq!(i.row(0), &[0xA5]);
    }
}
