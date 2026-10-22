//! Device-independent geometry.
//!
//! All sizes are in logical pixels (DIPs). The surface scale factor converts to
//! physical pixels at raster time, so layout never has to think about DPI.

use std::ops::{Add, AddAssign, Sub};

/// A point in local or surface space, logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, rhs: Point) -> Point {
        Point::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, rhs: Point) -> Point {
        Point::new(self.x - rhs.x, self.y - rhs.y)
    }
}

/// A size in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

impl Size {
    pub const ZERO: Size = Size { w: 0.0, h: 0.0 };

    pub const fn new(w: f32, h: f32) -> Self {
        Self { w, h }
    }

    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// Largest size component-wise.
    pub fn max(self, other: Size) -> Size {
        Size::new(self.w.max(other.w), self.h.max(other.h))
    }

    /// Smallest size component-wise.
    pub fn min(self, other: Size) -> Size {
        Size::new(self.w.min(other.w), self.h.min(other.h))
    }
}

/// An axis-aligned rectangle, logical pixels. `w`/`h` are never negative.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn from_size(size: Size) -> Self {
        Self::new(0.0, 0.0, size.w, size.h)
    }

    pub fn from_ltrb(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }

    pub fn left(self) -> f32 {
        self.x
    }
    pub fn top(self) -> f32 {
        self.y
    }
    pub fn right(self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    pub fn size(self) -> Size {
        Size::new(self.w, self.h)
    }

    pub fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn center(self) -> Point {
        Point::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.right() && p.y < self.bottom()
    }

    pub fn intersects(self, other: Rect) -> bool {
        self.x < other.right() && other.x < self.right() && self.y < other.bottom() && other.y < self.bottom()
    }

    /// Intersection, clamped to a possibly empty rect at the origin.
    pub fn intersect(self, other: Rect) -> Rect {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            Rect::ZERO
        } else {
            Rect::new(left, top, right - left, bottom - top)
        }
    }

    /// Smallest rect containing both.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(left, top, right - left, bottom - top)
    }

    pub fn inflate(self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x - dx, self.y - dy, self.w + dx * 2.0, self.h + dy * 2.0)
    }

    pub fn offset(self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Grows to cover every partial pixel, for rasterisation.
    pub fn snap_out(self) -> Rect {
        let left = self.x.floor();
        let top = self.y.floor();
        let right = self.right().ceil();
        let bottom = self.bottom().ceil();
        Rect::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }

    /// Converts to physical pixels for a given surface scale.
    pub fn scale(self, factor: f32) -> Rect {
        Rect::new(self.x * factor, self.y * factor, self.w * factor, self.h * factor)
    }
}

impl Add for Rect {
    type Output = Rect;
    fn add(self, rhs: Rect) -> Rect {
        self.union(rhs)
    }
}

/// Asymmetric spacing, logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Edges {
    pub const ZERO: Edges = Edges { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };

    pub const fn all(value: f32) -> Self {
        Self { left: value, top: value, right: value, bottom: value }
    }

    pub const fn symmetric(horizontal: f32, vertical: f32) -> Self {
        Self { left: horizontal, top: vertical, right: horizontal, bottom: vertical }
    }

    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self { left, top, right, bottom }
    }

    pub fn horizontal(self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(self) -> f32 {
        self.top + self.bottom
    }

    /// Shrinks a rect by these edges.
    pub fn deflate(self, rect: Rect) -> Rect {
        let w = (rect.w - self.horizontal()).max(0.0);
        let h = (rect.h - self.vertical()).max(0.0);
        Rect::new(rect.x + self.left, rect.y + self.top, w, h)
    }

    /// Grows a rect by these edges.
    pub fn inflate(self, rect: Rect) -> Rect {
        Rect::new(
            rect.x - self.left,
            rect.y - self.top,
            (rect.w + self.horizontal()).max(0.0),
            (rect.h + self.vertical()).max(0.0),
        )
    }
}

/// Flow direction for a container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Orientation {
    #[default]
    Vertical,
    Horizontal,
}

impl Orientation {
    pub fn is_horizontal(self) -> bool {
        matches!(self, Orientation::Horizontal)
    }
}

/// Alignment along either the main or cross axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

impl Align {
    /// Returns the offset of a child inside `available` given the child extent.
    pub fn offset(self, available: f32, extent: f32) -> f32 {
        match self {
            Align::Start | Align::Stretch => 0.0,
            Align::Center => ((available - extent) * 0.5).max(0.0),
            Align::End => (available - extent).max(0.0),
        }
    }
}

/// Minimal 2D affine transform, row-major `[a b c d tx ty]`.
///
/// Only translation and uniform scale are produced by layout today, but the
/// type exists so scrolled/zoomed subtrees do not need a second code path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub tx: f32,
    pub ty: f32,
    pub scale: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self { tx: 0.0, ty: 0.0, scale: 1.0 }
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform { tx: 0.0, ty: 0.0, scale: 1.0 };

    pub fn translate(tx: f32, ty: f32) -> Self {
        Self { tx, ty, scale: 1.0 }
    }

    pub fn apply(self, p: Point) -> Point {
        Point::new(p.x * self.scale + self.tx, p.y * self.scale + self.ty)
    }

    pub fn apply_rect(self, r: Rect) -> Rect {
        Rect::new(r.x * self.scale + self.tx, r.y * self.scale + self.ty, r.w * self.scale, r.h * self.scale)
    }

    /// `self ∘ inner`: applies `inner` first, then `self`.
    pub fn then(self, inner: Transform) -> Transform {
        Transform {
            tx: inner.tx * self.scale + self.tx,
            ty: inner.ty * self.scale + self.ty,
            scale: self.scale * inner.scale,
        }
    }
}

impl AddAssign for Point {
    fn add_assign(&mut self, rhs: Point) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_clamps_to_empty() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(20.0, 20.0, 5.0, 5.0);
        assert!(a.intersect(b).is_empty());
        let c = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(c), Rect::new(5.0, 5.0, 5.0, 5.0));
    }

    #[test]
    fn union_ignores_empty() {
        let a = Rect::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(a.union(Rect::ZERO), a);
        assert_eq!(Rect::ZERO.union(a), a);
    }

    #[test]
    fn edges_deflate_clamps() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        let out = Edges::all(6.0).deflate(r);
        assert_eq!(out.w, 0.0);
        assert_eq!(out.h, 0.0);
    }

    #[test]
    fn align_offsets() {
        assert_eq!(Align::Start.offset(100.0, 20.0), 0.0);
        assert_eq!(Align::Center.offset(100.0, 20.0), 40.0);
        assert_eq!(Align::End.offset(100.0, 20.0), 80.0);
        assert_eq!(Align::Stretch.offset(100.0, 20.0), 0.0);
    }
}
