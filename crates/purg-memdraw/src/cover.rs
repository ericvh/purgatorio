//! Coverage masks: the pixels a shape covers, within a rectangle (the
//! destination's clip, so a huge shape costs no more memory than the
//! image it is drawn on), and the integer rasterizers that fill them.

use alloc::vec;
use alloc::vec::Vec;

use crate::geom::{Point, Rect};

/// Subpixel units per pixel for polygon vertices.
pub(crate) const F: i64 = 256;

pub(crate) struct Cover {
    pub r: Rect,
    bits: Vec<bool>,
}

impl Cover {
    /// An empty mask over `r`, or over nothing when `r` is `None`.
    pub fn new(r: Option<Rect>) -> Cover {
        let r = r.unwrap_or_default();
        let n = if r.is_empty() {
            0
        } else {
            r.dx() as usize * r.dy() as usize
        };
        Cover {
            r,
            bits: vec![false; n],
        }
    }

    fn index(&self, p: Point) -> usize {
        (p.y - self.r.min.y) as usize * self.r.dx() as usize + (p.x - self.r.min.x) as usize
    }

    pub fn get(&self, p: Point) -> bool {
        self.r.contains(p) && self.bits[self.index(p)]
    }

    /// Covers `p`; points outside the mask's rectangle are ignored.
    pub fn set(&mut self, x: i64, y: i64) {
        if self.holds(x, y) {
            let i = self.index(Point::new(x as i32, y as i32));
            self.bits[i] = true;
        }
    }

    fn holds(&self, x: i64, y: i64) -> bool {
        x >= i64::from(self.r.min.x)
            && x < i64::from(self.r.max.x)
            && y >= i64::from(self.r.min.y)
            && y < i64::from(self.r.max.y)
    }

    /// Covers (or, with `on` false, uncovers) `x0 <= x < x1` on row `y`.
    pub fn span(&mut self, y: i64, x0: i64, x1: i64, on: bool) {
        if y < i64::from(self.r.min.y) || y >= i64::from(self.r.max.y) {
            return;
        }
        let x0 = x0.max(i64::from(self.r.min.x));
        let x1 = x1.min(i64::from(self.r.max.x));
        if x0 >= x1 {
            return;
        }
        let i = self.index(Point::new(x0 as i32, y as i32));
        self.bits[i..i + (x1 - x0) as usize].fill(on);
    }

    /// The rows the mask holds.
    pub fn rows(&self) -> core::ops::Range<i64> {
        i64::from(self.r.min.y)..i64::from(self.r.max.y)
    }

    /// Fills a closed polygon whose vertices are in subpixel units
    /// ([`F`] to a pixel; pixel `(x, y)` spans `x·F` to `(x+1)·F`). A
    /// pixel is covered when its centre is inside: when the winding
    /// number there, ANDed with `wind`, is not zero (draw(2): `~0` is
    /// the nonzero rule, 1 the odd-even rule). An edge holds the points
    /// at its top but not its bottom, and a span the points at its left
    /// but not its right, so polygons that share an edge do not overlap.
    pub fn fill_polygon(&mut self, pts: &[(i64, i64)], wind: i32) {
        if pts.len() < 3 {
            return;
        }
        let mut xs: Vec<(i64, i32)> = Vec::new();
        for y in self.rows() {
            let yc = y * F + F / 2;
            xs.clear();
            for i in 0..pts.len() {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                if a.1 == b.1 {
                    continue;
                }
                let (lo, hi, dir) = if a.1 < b.1 { (a, b, 1) } else { (b, a, -1) };
                if yc < lo.1 || yc >= hi.1 {
                    continue;
                }
                let num = i128::from(hi.0 - lo.0) * i128::from(yc - lo.1);
                let x = lo.0 + num.div_euclid(i128::from(hi.1 - lo.1)) as i64;
                xs.push((x, dir));
            }
            xs.sort_unstable();
            let mut w = 0i32;
            for k in 0..xs.len() {
                w += xs[k].1;
                if w & wind != 0 && k + 1 < xs.len() {
                    // Pixels whose centres lie in [xs[k], xs[k+1]).
                    let x0 = ceil_div(xs[k].0 - F / 2, F);
                    let x1 = ceil_div(xs[k + 1].0 - F / 2, F);
                    self.span(y, x0, x1, true);
                }
            }
        }
    }

    /// Covers the pixels of a one-pixel line from `p0` to `p1`, both
    /// included: one pixel per step along the longer axis, the other
    /// coordinate rounded to nearest (halves up). The pixels are the same
    /// whichever end the line starts from.
    pub fn thin_line(&mut self, p0: Point, p1: Point) {
        let (x0, y0) = (i64::from(p0.x), i64::from(p0.y));
        let (dx, dy) = (i64::from(p1.x) - x0, i64::from(p1.y) - y0);
        let n = dx.abs().max(dy.abs());
        if n == 0 {
            self.set(x0, y0);
            return;
        }
        // Only the steps whose major coordinate falls in the mask.
        let (major0, step, lo, hi) = if dx.abs() >= dy.abs() {
            (x0, dx.signum(), self.r.min.x, self.r.max.x)
        } else {
            (y0, dy.signum(), self.r.min.y, self.r.max.y)
        };
        let (lo, hi) = (i64::from(lo), i64::from(hi) - 1);
        let (a, b) = if step > 0 {
            (lo - major0, hi - major0)
        } else {
            (major0 - hi, major0 - lo)
        };
        let round = |v: i64| (2 * v + n).div_euclid(2 * n);
        for i in a.max(0)..=b.min(n) {
            let (x, y) = if dx.abs() >= dy.abs() {
                (x0 + i * step, y0 + round(i * dy))
            } else {
                (x0 + round(i * dx), y0 + i * step)
            };
            self.set(x, y);
        }
    }

    /// Covers a filled ellipse centred on `c` with semiaxes `a` and `b`
    /// (see [`half_width`]), or the part of it in `sector`.
    pub fn fill_ellipse(&mut self, c: Point, a: i64, b: i64, sector: Option<&Sector>) {
        let (cx, cy) = (i64::from(c.x), i64::from(c.y));
        for y in self.rows() {
            let Some(w) = half_width(a, b, y - cy) else {
                continue;
            };
            self.span_in(y, cx, -w, w, cy, sector);
        }
    }

    /// Covers the outline of an ellipse `1 + 2·t` pixels thick: the
    /// filled ellipse with semiaxes `a+t`, `b+t`, less the inside of the
    /// one with semiaxes `a-t`, `b-t` (its pixels whose four neighbours
    /// are all in it). With `t` 0 that is the filled ellipse's boundary.
    pub fn ellipse(&mut self, c: Point, a: i64, b: i64, t: i64, sector: Option<&Sector>) {
        let (cx, cy) = (i64::from(c.x), i64::from(c.y));
        let (ia, ib) = (a - t, b - t);
        for y in self.rows() {
            let dy = y - cy;
            let Some(w) = half_width(a + t, b + t, dy) else {
                continue;
            };
            let inner = if ia >= 0 && ib >= 0 {
                match (
                    half_width(ia, ib, dy),
                    half_width(ia, ib, dy - 1),
                    half_width(ia, ib, dy + 1),
                ) {
                    (Some(m), Some(u), Some(d)) => Some((m - 1).min(u).min(d)),
                    _ => None,
                }
            } else {
                None
            };
            match inner {
                Some(m) if m >= 0 => {
                    self.span_in(y, cx, -w, -m - 1, cy, sector);
                    self.span_in(y, cx, m + 1, w, cy, sector);
                }
                _ => self.span_in(y, cx, -w, w, cy, sector),
            }
        }
    }

    /// Covers `cx+x0 ..= cx+x1` on row `y`, keeping to `sector` (about
    /// the centre `(cx, cy)`) when there is one.
    fn span_in(&mut self, y: i64, cx: i64, x0: i64, x1: i64, cy: i64, sector: Option<&Sector>) {
        match sector {
            None => self.span(y, cx + x0, cx + x1 + 1, true),
            Some(s) => {
                let lo = (cx + x0).max(i64::from(self.r.min.x));
                let hi = (cx + x1).min(i64::from(self.r.max.x) - 1);
                for x in lo..=hi {
                    if s.holds(x - cx, cy - y) {
                        self.set(x, y);
                    }
                }
            }
        }
    }
}

/// `ceil(a / b)` for `b > 0`.
pub(crate) fn ceil_div(a: i64, b: i64) -> i64 {
    -((-a).div_euclid(b))
}

/// The integer square root of `n`.
pub(crate) fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    // Newton's method from above.
    let mut x = 1u128 << (128 - n.leading_zeros()).div_ceil(2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Half the width of row `dy` (relative to the centre) of the filled
/// ellipse with semiaxes `a` and `b`, or `None` when the row is outside
/// it. A pixel `(x, y)` is inside when it lies in the ellipse with
/// semiaxes `a + ½` and `b + ½`, so an ellipse with semiaxis `a` is
/// `2a + 1` pixels across and one with both semiaxes 0 is one pixel.
pub(crate) fn half_width(a: i64, b: i64, dy: i64) -> Option<i64> {
    if a < 0 || b < 0 {
        return None;
    }
    // 4x²(2b+1)² + 4y²(2a+1)² <= (2a+1)²(2b+1)²
    let a2 = (2 * i128::from(a) + 1).pow(2);
    let b2 = (2 * i128::from(b) + 1).pow(2);
    let rest = a2 * b2 - 4 * i128::from(dy) * i128::from(dy) * a2;
    if rest < 0 {
        return None;
    }
    Some(isqrt((rest / (4 * b2)) as u128) as i64)
}

/// sin(d°) for d in 0..=90, scaled by 2^16.
const SIN: [i64; 91] = [
    0, 1144, 2287, 3430, 4572, 5712, 6850, 7987, 9121, 10252, 11380, 12505, 13626, 14742, 15855,
    16962, 18064, 19161, 20252, 21336, 22415, 23486, 24550, 25607, 26656, 27697, 28729, 29753,
    30767, 31772, 32768, 33754, 34729, 35693, 36647, 37590, 38521, 39441, 40348, 41243, 42126,
    42995, 43852, 44695, 45525, 46341, 47143, 47930, 48703, 49461, 50203, 50931, 51643, 52339,
    53020, 53684, 54332, 54963, 55578, 56175, 56756, 57319, 57865, 58393, 58903, 59396, 59870,
    60326, 60764, 61183, 61584, 61966, 62328, 62672, 62997, 63303, 63589, 63856, 64104, 64332,
    64540, 64729, 64898, 65048, 65177, 65287, 65376, 65446, 65496, 65526, 65536,
];

/// `(cos d°, sin d°)` scaled by 2^16, for any whole number of degrees.
fn dir(d: i64) -> (i64, i64) {
    let d = d.rem_euclid(360) as usize;
    let s = |k: usize| SIN[k];
    match d {
        0..=90 => (s(90 - d), s(d)),
        91..=180 => (-s(d - 90), s(180 - d)),
        181..=270 => (-s(270 - d), -s(d - 180)),
        _ => (s(d - 270), -s(360 - d)),
    }
}

/// The wedge of an arc: from angle `alpha` counterclockwise through
/// `phi` degrees, angles measured from the x axis with y up (draw(2)).
pub(crate) struct Sector {
    start: (i64, i64),
    end: (i64, i64),
    wide: bool,
    full: bool,
}

impl Sector {
    pub fn new(alpha: i32, phi: i32) -> Sector {
        let (mut a, mut p) = (i64::from(alpha), i64::from(phi));
        if p < 0 {
            a += p;
            p = -p;
        }
        Sector {
            start: dir(a),
            end: dir(a + p),
            wide: p > 180,
            full: p >= 360,
        }
    }

    /// Whether the point `(x, y)` (y up, from the centre) is in the wedge.
    pub fn holds(&self, x: i64, y: i64) -> bool {
        if self.full || (x == 0 && y == 0) {
            return true;
        }
        let cross = |a: (i64, i64), b: (i64, i64)| a.0 * b.1 - a.1 * b.0;
        let q = (x, y);
        if self.wide {
            !(cross(self.end, q) > 0 && cross(q, self.start) > 0)
        } else {
            cross(self.start, q) >= 0 && cross(q, self.end) >= 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots() {
        for n in [0u128, 1, 2, 3, 4, 15, 16, 17, 1 << 40, (1 << 62) + 12345] {
            let r = isqrt(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "{n}");
        }
    }

    #[test]
    fn ellipse_widths() {
        assert_eq!(half_width(0, 0, 0), Some(0));
        assert_eq!(half_width(0, 0, 1), None);
        assert_eq!(half_width(5, 3, 0), Some(5));
        assert_eq!(half_width(5, 3, 3), Some(2));
        assert_eq!(half_width(5, 3, 4), None);
    }

    #[test]
    fn directions() {
        assert_eq!(dir(0), (65536, 0));
        assert_eq!(dir(90), (0, 65536));
        assert_eq!(dir(180), (-65536, 0));
        assert_eq!(dir(270), (0, -65536));
        assert_eq!(dir(-90), (0, -65536));
        let s = Sector::new(0, 90);
        assert!(s.holds(5, 5) && !s.holds(-5, 5) && !s.holds(5, -5));
        let s = Sector::new(0, 270);
        assert!(s.holds(-5, 5) && s.holds(-5, -5) && !s.holds(5, -5));
    }
}
