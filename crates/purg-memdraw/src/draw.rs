//! Compositing: draw(2)'s `draw` and `gendraw` with Porter-Duff
//! operators.
//!
//! The source is first taken "in" the mask (its colour and alpha scaled
//! by the mask's alpha, or by its grey level when it has no alpha), and
//! the result is composited onto the destination:
//! `dst = src·Fs + dst·Fd`, where `Fs` and `Fd` come from the operator's
//! four bits (`SinD`, `SoutD`, `DinS`, `DoutS`). All of it is on
//! premultiplied 8-bit values, with products rounded to nearest.

use crate::cover::Cover;
use crate::geom::{Point, Rect};
use crate::image::Image;

/// A compositing operator, draw(2)'s `Drawop`: a set of the four
/// Porter-Duff regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op(pub u8);

impl Op {
    pub const CLEAR: Op = Op(0);
    pub const DOUT_S: Op = Op(1);
    pub const SOUT_D: Op = Op(2);
    pub const SXOR_D: Op = Op(3);
    pub const DXOR_S: Op = Op(3);
    pub const DIN_S: Op = Op(4);
    pub const D: Op = Op(5);
    pub const DATOP_S: Op = Op(6);
    pub const DOVER_S: Op = Op(7);
    pub const SIN_D: Op = Op(8);
    pub const SATOP_D: Op = Op(9);
    pub const S: Op = Op(10);
    pub const SOVER_D: Op = Op(11);

    /// The operator a wire byte names; only the four region bits count.
    pub fn from_byte(b: u8) -> Op {
        Op(b & 15)
    }

    fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// `x·y/255`, rounded to nearest.
#[inline]
pub(crate) fn mul(x: u32, y: u32) -> u32 {
    let t = x * y + 128;
    (t + (t >> 8)) >> 8
}

/// Composites one premultiplied source pixel, already in its mask with
/// mask value `m`, onto destination pixel `d`.
#[inline]
pub(crate) fn blend(s: [u8; 4], m: u32, d: [u8; 4], op: Op) -> [u8; 4] {
    let s = [
        mul(u32::from(s[0]), m),
        mul(u32::from(s[1]), m),
        mul(u32::from(s[2]), m),
        mul(u32::from(s[3]), m),
    ];
    let (sa, da) = (s[3], u32::from(d[3]));
    let fs = match (op.has(8), op.has(2)) {
        (true, true) => 255,
        (true, false) => da,
        (false, true) => 255 - da,
        (false, false) => 0,
    };
    let fd = match (op.has(4), op.has(1)) {
        (true, true) => 255,
        (true, false) => sa,
        (false, true) => 255 - sa,
        (false, false) => 0,
    };
    let mut out = [0u8; 4];
    for i in 0..4 {
        out[i] = (mul(s[i], fs) + mul(u32::from(d[i]), fd)).min(255) as u8;
    }
    out
}

/// What shapes the source: an image's alpha (or grey), or a coverage
/// mask built by the geometry.
pub(crate) enum Mask<'a> {
    /// Every pixel is fully covered.
    Opaque,
    /// The mask image, and the offset from a destination point to its
    /// point in the mask.
    Image(&'a Image, Point),
    Cover(&'a Cover),
}

/// The point of `img` that `p` reads, after replication, or `None` when
/// `p` is outside what the image may supply.
#[inline]
fn sample(img: &Image, p: Point) -> Option<Point> {
    if !img.clipr.contains(p) {
        return None;
    }
    if !img.repl {
        return img.r.contains(p).then_some(p);
    }
    let r = img.r;
    Some(Point::new(
        r.min.x + (p.x - r.min.x).rem_euclid(r.dx()),
        r.min.y + (p.y - r.min.y).rem_euclid(r.dy()),
    ))
}

/// The value a mask pixel scales the source by.
#[inline]
fn mask_value(img: &Image, p: Point) -> u32 {
    let c = img.rgba(p);
    if img.layout().alpha {
        u32::from(c[3])
    } else {
        u32::from(crate::chan::grey(c[0], c[1], c[2]))
    }
}

/// A replicated 1×1 image: one colour everywhere its clip allows.
fn solid(img: &Image) -> bool {
    img.repl && img.r.dx() == 1 && img.r.dy() == 1
}

/// Composites `src` through `mask` onto `r` of `dst`. `r` must lie in
/// `dst.r`; source point = destination point + `sd`. Points the source or
/// mask cannot supply are left alone.
pub(crate) fn composite(dst: &mut Image, r: Rect, src: &Image, sd: Point, mask: &Mask, op: Op) {
    if r.is_empty() {
        return;
    }
    if let Mask::Image(m, md) = mask
        && solid(m)
        && m.clipr.contains(r.min + *md)
        && m.clipr.contains(r.max - Point::new(1, 1) + *md)
        && mask_value(m, m.r.min) == 255
    {
        return composite(dst, r, src, sd, &Mask::Opaque, op);
    }
    if matches!(mask, Mask::Opaque) && fast(dst, r, src, sd, op) {
        return;
    }
    for y in r.min.y..r.max.y {
        for x in r.min.x..r.max.x {
            let p = Point::new(x, y);
            let m = match mask {
                Mask::Opaque => 255,
                Mask::Image(img, md) => match sample(img, p + *md) {
                    Some(q) => mask_value(img, q),
                    None => continue,
                },
                Mask::Cover(c) => {
                    if !c.get(p) {
                        continue;
                    }
                    255
                }
            };
            let Some(q) = sample(src, p + sd) else {
                continue;
            };
            let s = src.rgba(q);
            if m == 255 && (op == Op::S || (op == Op::SOVER_D && s[3] == 255)) {
                let v = dst.pack(s);
                dst.set_raw(p, v);
                continue;
            }
            let out = blend(s, m, dst.rgba(p), op);
            let v = dst.pack(out);
            dst.set_raw(p, v);
        }
    }
}

/// Fast paths for an opaque mask: a solid colour, and a copy between
/// images with the same channels. Returns false when neither applies.
fn fast(dst: &mut Image, r: Rect, src: &Image, sd: Point, op: Op) -> bool {
    if op != Op::S && op != Op::SOVER_D {
        return false;
    }
    if solid(src) {
        if !(src.clipr.contains(r.min + sd) && src.clipr.contains(r.max - Point::new(1, 1) + sd)) {
            return false;
        }
        let c = src.rgba(src.r.min);
        if op == Op::SOVER_D && c[3] != 255 {
            return false;
        }
        let v = dst.pack(c);
        dst.fill_raw(r, v);
        return true;
    }
    let sr = r.add(sd);
    let d = dst.depth();
    let opaque = op == Op::S || !src.layout().alpha;
    if src.repl
        || src.chan() != dst.chan()
        || !opaque
        || !sr.inside(&src.r)
        || !sr.inside(&src.clipr)
    {
        return false;
    }
    if d < 8 {
        let d = i64::from(d);
        let ok = |x: i32| i64::from(x) * d % 8 == 0;
        if !(ok(r.min.x) && ok(r.max.x) && ok(sr.min.x)) {
            return false;
        }
    }
    dst.copy_rows(r, src, sr.min);
    true
}

/// draw(2)'s `draw`: composites `src`, aligned so that `sp` falls on
/// `r.min`, through `mask` (aligned with `mp`; `None` is opaque) onto
/// `r` of `dst`. Clips to the destination's rectangle and clip, and to
/// the source's and mask's (their rectangles too, unless replicated), as
/// draw(2) does. Returns the rectangle of `dst` it changed.
pub fn draw(
    dst: &mut Image,
    r: Rect,
    src: &Image,
    sp: Point,
    mask: Option<&Image>,
    mp: Point,
    op: Op,
) -> Option<Rect> {
    let (r, sp, mp) = clip(dst, r, src, sp, mask, mp)?;
    let sd = sp - r.min;
    match mask {
        Some(m) => composite(dst, r, src, sd, &Mask::Image(m, mp - r.min), op),
        None => composite(dst, r, src, sd, &Mask::Opaque, op),
    }
    Some(r)
}

/// draw(2)'s clipping: shrinks `r` and moves `sp` and `mp` with it.
fn clip(
    dst: &Image,
    r0: Rect,
    src: &Image,
    sp: Point,
    mask: Option<&Image>,
    mp: Point,
) -> Option<(Rect, Point, Point)> {
    let mut r = r0.clip(&dst.r)?.clip(&dst.clipr)?;
    let (mut sp, mut mp) = (sp + (r.min - r0.min), mp + (r.min - r0.min));
    // Clips r to what `img` can supply when `p` falls on r.min.
    let limit = |r: &mut Rect, p: Point, img: &Image| -> Option<Point> {
        let lim = if img.repl {
            img.clipr
        } else {
            img.clipr.clip(&img.r)?
        };
        let ir = r.sub(r.min).add(p);
        let nr = ir.clip(&lim)?;
        let delta = nr.min - ir.min;
        *r = nr.sub(p).add(r.min);
        Some(delta)
    };
    let delta = limit(&mut r, sp, src)?;
    sp = sp + delta;
    mp = mp + delta;
    if let Some(m) = mask {
        let delta = limit(&mut r, mp, m)?;
        sp = sp + delta;
        mp = mp + delta;
    }
    Some((r, sp, mp))
}
