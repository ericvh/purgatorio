//! draw(2)'s geometry: lines, polygons, ellipses and arcs.
//!
//! Each builds a coverage mask with integer arithmetic and composites the
//! source through it with the operator, so a translucent shape covers
//! each pixel once even where its pieces overlap.
//!
//! The conventions, from draw(2) where it says and our own reading where
//! it does not:
//!
//! - A line's end points are pixels; it is `1 + 2·thick` pixels wide and
//!   covers both end points. With `thick` 0 it is one pixel per step
//!   along its longer axis.
//! - `Endsquare` ends a thick line square, half a pixel past the end
//!   point's centre (a horizontal line from x0 to x1 covers x0 to x1);
//!   `Enddisc` adds a disc of radius `thick` at the end; `Endarrow` adds
//!   an arrow head (`ARROW(a, b, c)`: tip to notch along the line, tip to
//!   barbs along the line, barbs beyond the line's edge across it).
//! - A polygon's vertices for `fillpoly` are pixel corners: the polygon
//!   (0,0) (10,0) (10,10) (0,10) fills 10 by 10 pixels.
//! - An ellipse with semiaxes `a` and `b` is `2a+1` by `2b+1` pixels.
//! - The source is aligned with the first point (a line's `p0`, a
//!   polygon's first vertex, an ellipse's centre).

use crate::cover::{Cover, F, Sector, isqrt};
use crate::draw::{Mask, Op, composite};
use crate::geom::{Point, Rect};
use crate::image::Image;

pub const ENDSQUARE: i32 = 0;
pub const ENDDISC: i32 = 1;
pub const ENDARROW: i32 = 2;
/// The bits of an end value that name the style; the rest are an
/// arrow's sizes.
pub const ENDMASK: i32 = 0x1F;

/// An arrow head's sizes when the end value gives none.
const ARROW_DEFAULT: (i64, i64, i64) = (8, 10, 5);

fn arrow_size(end: i32) -> (i64, i64, i64) {
    let a = i64::from((end >> 5) & 0x1FF);
    let b = i64::from((end >> 14) & 0x1FF);
    let c = i64::from((end >> 23) & 0x1FF);
    if a == 0 && b == 0 && c == 0 {
        ARROW_DEFAULT
    } else {
        (a, b, c)
    }
}

/// `round(a / b)` for `b > 0`, halves up.
fn rdiv(a: i128, b: i128) -> i64 {
    (2 * a + b).div_euclid(2 * b) as i64
}

/// The centre of pixel `p` in subpixel units.
fn centre(p: Point) -> (i64, i64) {
    (i64::from(p.x) * F + F / 2, i64::from(p.y) * F + F / 2)
}

/// The destination's drawable rectangle, intersected with `[x0, x1) ×
/// [y0, y1)` (in i64, so that margins cannot overflow).
fn bound(dst: &Image, x0: i64, y0: i64, x1: i64, y1: i64) -> Option<Rect> {
    let c = dst.r.clip(&dst.clipr)?;
    let r = Rect::new(
        x0.clamp(i64::from(c.min.x), i64::from(c.max.x)) as i32,
        y0.clamp(i64::from(c.min.y), i64::from(c.max.y)) as i32,
        x1.clamp(i64::from(c.min.x), i64::from(c.max.x)) as i32,
        y1.clamp(i64::from(c.min.y), i64::from(c.max.y)) as i32,
    );
    (!r.is_empty()).then_some(r)
}

/// Composites `src` through `cover` and returns what changed.
fn finish(dst: &mut Image, cover: &Cover, src: &Image, sd: Point, op: Op) -> Option<Rect> {
    if cover.r.is_empty() {
        return None;
    }
    composite(dst, cover.r, src, sd, &Mask::Cover(cover), op);
    Some(cover.r)
}

/// A vector `v` scaled to length `k` (subpixel units).
fn scaled(v: (i64, i64), len: i64, k: i64) -> (i64, i64) {
    (
        rdiv(i128::from(v.0) * i128::from(k), i128::from(len)),
        rdiv(i128::from(v.1) * i128::from(k), i128::from(len)),
    )
}

/// Covers a thick segment from `p0` to `p1`, `1 + 2·t` pixels wide,
/// reaching `ext0` and `ext1` subpixel units past the end points' centres
/// (negative to stop short).
fn segment(cover: &mut Cover, p0: Point, p1: Point, t: i64, ext0: i64, ext1: i64) {
    let (c0, c1) = (centre(p0), centre(p1));
    let d = (c1.0 - c0.0, c1.1 - c0.1);
    let len = isqrt((i128::from(d.0).pow(2) + i128::from(d.1).pow(2)) as u128) as i64;
    if len == 0 {
        let (x, y) = (i64::from(p0.x), i64::from(p0.y));
        for row in y - t..=y + t {
            cover.span(row, x - t, x + t + 1, true);
        }
        return;
    }
    let h = t * F + F / 2;
    let o = scaled((-d.1, d.0), len, h);
    let e0 = scaled(d, len, ext0);
    let e1 = scaled(d, len, ext1);
    let a = (c0.0 - e0.0, c0.1 - e0.1);
    let b = (c1.0 + e1.0, c1.1 + e1.1);
    cover.fill_polygon(
        &[
            (a.0 + o.0, a.1 + o.1),
            (b.0 + o.0, b.1 + o.1),
            (b.0 - o.0, b.1 - o.1),
            (a.0 - o.0, a.1 - o.1),
        ],
        !0,
    );
}

/// Covers an arrow head at `p` for a line arriving from `q`; returns how
/// far before `p` the line itself should stop (a negative extension).
fn arrow(cover: &mut Cover, q: Point, p: Point, t: i64, end: i32) -> i64 {
    let (a, b, c) = arrow_size(end);
    let (cq, tip) = (centre(q), centre(p));
    let d = (tip.0 - cq.0, tip.1 - cq.1);
    let len = isqrt((i128::from(d.0).pow(2) + i128::from(d.1).pow(2)) as u128) as i64;
    if len == 0 {
        return 0;
    }
    let back = scaled(d, len, b * F);
    let notch = scaled(d, len, a * F);
    let o = scaled((-d.1, d.0), len, c * F + t * F + F / 2);
    let base = (tip.0 - back.0, tip.1 - back.1);
    cover.fill_polygon(
        &[
            tip,
            (base.0 + o.0, base.1 + o.1),
            (tip.0 - notch.0, tip.1 - notch.1),
            (base.0 - o.0, base.1 - o.1),
        ],
        !0,
    );
    -a * F
}

/// Covers one end of a polyline: `p` is the end, `q` its neighbour.
/// Returns the extension for the segment that ends there.
fn cap(cover: &mut Cover, q: Point, p: Point, t: i64, end: i32) -> i64 {
    match end & ENDMASK {
        ENDDISC => {
            cover.fill_ellipse(p, t, t, None);
            0
        }
        ENDARROW => arrow(cover, q, p, t, end),
        _ => F / 2,
    }
}

/// draw(2)'s `poly`: line segments joining `pts`, `1 + 2·thick` pixels
/// wide, with `end0` and `end1` at the first and last points and discs
/// where segments meet. The source is aligned so that `sp` falls on
/// `pts[0]`.
#[allow(clippy::too_many_arguments)]
pub fn poly(
    dst: &mut Image,
    pts: &[Point],
    end0: i32,
    end1: i32,
    thick: i32,
    src: &Image,
    sp: Point,
    op: Op,
) -> Option<Rect> {
    let first = *pts.first()?;
    if thick < 0 {
        return None;
    }
    let t = i64::from(thick);
    let margin = t + 2 + {
        let s = |e: i32| {
            if e & ENDMASK == ENDARROW {
                let (a, b, c) = arrow_size(e);
                a.max(b) + c
            } else {
                0
            }
        };
        s(end0).max(s(end1))
    };
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for p in pts {
        x0 = x0.min(i64::from(p.x));
        y0 = y0.min(i64::from(p.y));
        x1 = x1.max(i64::from(p.x));
        y1 = y1.max(i64::from(p.y));
    }
    let mut cover = Cover::new(bound(
        dst,
        x0 - margin,
        y0 - margin,
        x1 + margin + 1,
        y1 + margin + 1,
    ));
    if cover.r.is_empty() {
        return None;
    }
    let n = pts.len();
    if n == 1 {
        segment(&mut cover, first, first, t, 0, 0);
        return finish(dst, &cover, src, sp - first, op);
    }
    let ext0 = cap(&mut cover, pts[1], pts[0], t, end0);
    let ext1 = cap(&mut cover, pts[n - 2], pts[n - 1], t, end1);
    for i in 0..n - 1 {
        if t == 0 {
            cover.thin_line(pts[i], pts[i + 1]);
            continue;
        }
        let e0 = if i == 0 { ext0 } else { 0 };
        let e1 = if i == n - 2 { ext1 } else { 0 };
        segment(&mut cover, pts[i], pts[i + 1], t, e0, e1);
        if i > 0 {
            cover.fill_ellipse(pts[i], t, t, None);
        }
    }
    finish(dst, &cover, src, sp - first, op)
}

/// draw(2)'s `line`: a polyline of one segment.
#[allow(clippy::too_many_arguments)]
pub fn line(
    dst: &mut Image,
    p0: Point,
    p1: Point,
    end0: i32,
    end1: i32,
    thick: i32,
    src: &Image,
    sp: Point,
    op: Op,
) -> Option<Rect> {
    poly(dst, &[p0, p1], end0, end1, thick, src, sp, op)
}

/// draw(2)'s `fillpoly`: fills the polygon `pts` (closed back to the
/// first point) by the winding rule `wind` (`~0` nonzero, 1 odd-even).
/// The source is aligned so that `sp` falls on `pts[0]`.
pub fn fill_poly(
    dst: &mut Image,
    pts: &[Point],
    wind: i32,
    src: &Image,
    sp: Point,
    op: Op,
) -> Option<Rect> {
    let first = *pts.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    let mut v = alloc::vec::Vec::with_capacity(pts.len());
    for p in pts {
        let (x, y) = (i64::from(p.x), i64::from(p.y));
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
        v.push((x * F, y * F));
    }
    let mut cover = Cover::new(bound(dst, x0, y0, x1, y1));
    cover.fill_polygon(&v, wind);
    finish(dst, &cover, src, sp - first, op)
}

fn ellipse_cover(dst: &Image, c: Point, a: i32, b: i32, t: i32) -> Option<Cover> {
    if a < 0 || b < 0 || t < 0 {
        return None;
    }
    let (cx, cy) = (i64::from(c.x), i64::from(c.y));
    let (ea, eb) = (i64::from(a) + i64::from(t), i64::from(b) + i64::from(t));
    Some(Cover::new(Some(bound(
        dst,
        cx - ea,
        cy - eb,
        cx + ea + 1,
        cy + eb + 1,
    )?)))
}

/// draw(2)'s `ellipse`: the outline of the ellipse centred on `c` with
/// semiaxes `a` and `b`, `1 + 2·thick` pixels wide. The source is aligned
/// so that `sp` falls on `c`.
#[allow(clippy::too_many_arguments)]
pub fn ellipse(
    dst: &mut Image,
    c: Point,
    a: i32,
    b: i32,
    thick: i32,
    src: &Image,
    sp: Point,
    op: Op,
) -> Option<Rect> {
    let mut cover = ellipse_cover(dst, c, a, b, thick)?;
    cover.ellipse(c, a.into(), b.into(), thick.into(), None);
    finish(dst, &cover, src, sp - c, op)
}

/// draw(2)'s `fillellipse`.
pub fn fill_ellipse(
    dst: &mut Image,
    c: Point,
    a: i32,
    b: i32,
    src: &Image,
    sp: Point,
    op: Op,
) -> Option<Rect> {
    let mut cover = ellipse_cover(dst, c, a, b, 0)?;
    cover.fill_ellipse(c, a.into(), b.into(), None);
    finish(dst, &cover, src, sp - c, op)
}

/// draw(2)'s `arc`: the part of [`ellipse`]'s outline from angle `alpha`
/// counterclockwise through `phi` degrees (from the x axis, y up).
#[allow(clippy::too_many_arguments)]
pub fn arc(
    dst: &mut Image,
    c: Point,
    a: i32,
    b: i32,
    thick: i32,
    src: &Image,
    sp: Point,
    alpha: i32,
    phi: i32,
    op: Op,
) -> Option<Rect> {
    let mut cover = ellipse_cover(dst, c, a, b, thick)?;
    cover.ellipse(
        c,
        a.into(),
        b.into(),
        thick.into(),
        Some(&Sector::new(alpha, phi)),
    );
    finish(dst, &cover, src, sp - c, op)
}

/// draw(2)'s `fillarc`: a pie slice of [`fill_ellipse`].
#[allow(clippy::too_many_arguments)]
pub fn fill_arc(
    dst: &mut Image,
    c: Point,
    a: i32,
    b: i32,
    src: &Image,
    sp: Point,
    alpha: i32,
    phi: i32,
    op: Op,
) -> Option<Rect> {
    let mut cover = ellipse_cover(dst, c, a, b, 0)?;
    cover.fill_ellipse(c, a.into(), b.into(), Some(&Sector::new(alpha, phi)));
    finish(dst, &cover, src, sp - c, op)
}
