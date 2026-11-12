//! Points and rectangles, as in draw(2): a rectangle holds the points
//! with `min.x <= x < max.x` and `min.y <= y < max.y`.

use core::ops::{Add, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Point {
        Point { x, y }
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x.wrapping_add(o.x), self.y.wrapping_add(o.y))
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x.wrapping_sub(o.x), self.y.wrapping_sub(o.y))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        Rect {
            min: Point::new(x0, y0),
            max: Point::new(x1, y1),
        }
    }

    /// A rectangle that holds every point a client can name; replicated
    /// images use it as an unbounded clip.
    pub const fn infinite() -> Rect {
        Rect::new(-0x3FFF_FFFF, -0x3FFF_FFFF, 0x3FFF_FFFF, 0x3FFF_FFFF)
    }

    pub fn dx(&self) -> i32 {
        self.max.x.wrapping_sub(self.min.x)
    }

    pub fn dy(&self) -> i32 {
        self.max.y.wrapping_sub(self.min.y)
    }

    /// Whether the rectangle holds no point.
    pub fn is_empty(&self) -> bool {
        self.min.x >= self.max.x || self.min.y >= self.max.y
    }

    /// The same rectangle with `min <= max` in each coordinate
    /// (draw(2)'s `canonrect`).
    pub fn canon(&self) -> Rect {
        Rect::new(
            self.min.x.min(self.max.x),
            self.min.y.min(self.max.y),
            self.min.x.max(self.max.x),
            self.min.y.max(self.max.y),
        )
    }

    pub fn add(&self, p: Point) -> Rect {
        Rect {
            min: self.min + p,
            max: self.max + p,
        }
    }

    pub fn sub(&self, p: Point) -> Rect {
        Rect {
            min: self.min - p,
            max: self.max - p,
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min.x && p.x < self.max.x && p.y >= self.min.y && p.y < self.max.y
    }

    /// Whether `self` lies inside `o` (draw(2)'s `rectinrect`).
    pub fn inside(&self, o: &Rect) -> bool {
        self.min.x >= o.min.x
            && self.min.y >= o.min.y
            && self.max.x <= o.max.x
            && self.max.y <= o.max.y
    }

    /// Whether the two rectangles share a point (`rectXrect`).
    pub fn overlaps(&self, o: &Rect) -> bool {
        self.min.x < o.max.x && o.min.x < self.max.x && self.min.y < o.max.y && o.min.y < self.max.y
    }

    /// The intersection, or `None` when the rectangles share no point
    /// (`rectclip`).
    pub fn clip(&self, o: &Rect) -> Option<Rect> {
        if !self.overlaps(o) {
            return None;
        }
        Some(Rect::new(
            self.min.x.max(o.min.x),
            self.min.y.max(o.min.y),
            self.max.x.min(o.max.x),
            self.max.y.min(o.max.y),
        ))
    }

    /// The smallest rectangle holding both (`combinerect`); an empty
    /// rectangle adds nothing.
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect::new(
            self.min.x.min(o.min.x),
            self.min.y.min(o.min.y),
            self.max.x.max(o.max.x),
            self.max.y.max(o.max.y),
        )
    }
}
