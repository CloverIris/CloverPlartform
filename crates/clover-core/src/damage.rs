//! Damage tracking.
//!
//! A surface reports which rectangles changed since the last presented frame.
//! The compositor only rasterises those rectangles, which is what makes a
//! 1600x3000 canvas cheap to keep at 120 Hz while typing in a text field.

use crate::geometry::Rect;

/// A growable set of dirty rectangles, coalesced on the way out.
#[derive(Clone, Debug, Default)]
pub struct Damage {
    rects: Vec<Rect>,
    full: bool,
}

/// Damage smaller than this many square logical pixels is merged instead of
/// tracked separately; it keeps the rect list short for cursor blinks.
const MERGE_AREA: f32 = 256.0;

impl Damage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the whole surface dirty.
    pub fn mark_full(&mut self) {
        self.full = true;
        self.rects.clear();
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn is_empty(&self) -> bool {
        !self.full && self.rects.is_empty()
    }

    pub fn add(&mut self, rect: Rect) {
        if self.full || rect.is_empty() {
            return;
        }
        if rect.w * rect.h >= MERGE_AREA {
            self.rects.push(rect);
        } else if let Some(last) = self.rects.last_mut() {
            *last = last.union(rect);
        } else {
            self.rects.push(rect);
        }
    }

    /// Adds every rect of `other`.
    pub fn add_damage(&mut self, other: &Damage) {
        if other.full {
            self.mark_full();
            return;
        }
        for r in &other.rects {
            self.add(*r);
        }
    }

    pub fn clear(&mut self) {
        self.full = false;
        self.rects.clear();
    }

    /// Takes the dirty rects, merged and clipped to `bounds`.
    ///
    /// Returns every rect when the whole surface is dirty.
    pub fn take(&mut self, bounds: Rect) -> Vec<Rect> {
        if self.full {
            self.full = false;
            self.rects.clear();
            return if bounds.is_empty() { Vec::new() } else { vec![bounds] };
        }
        let mut out: Vec<Rect> = Vec::with_capacity(self.rects.len());
        for r in self.rects.drain(..) {
            let clipped = if bounds.is_empty() { r } else { r.intersect(bounds) };
            if !clipped.is_empty() {
                out.push(clipped.snap_out());
            }
        }
        out
    }

    /// Number of tracked rects, for tests and diagnostics.
    pub fn len(&self) -> usize {
        self.rects.len()
    }
}

impl From<Rect> for Damage {
    fn from(rect: Rect) -> Self {
        let mut d = Damage::new();
        d.add(rect);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        assert!(Damage::new().is_empty());
    }

    #[test]
    fn full_reports_whole_bounds() {
        let mut d = Damage::new();
        d.mark_full();
        let bounds = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert_eq!(d.take(bounds), vec![bounds]);
        assert!(d.is_empty(), "take must clear");
    }

    #[test]
    fn small_rects_merge_into_previous() {
        let mut d = Damage::new();
        d.add(Rect::new(0.0, 0.0, 4.0, 4.0));
        d.add(Rect::new(8.0, 0.0, 4.0, 4.0));
        assert_eq!(d.len(), 1, "tiny rects coalesce");
    }

    #[test]
    fn large_rects_stay_separate() {
        let mut d = Damage::new();
        d.add(Rect::new(0.0, 0.0, 64.0, 64.0));
        d.add(Rect::new(128.0, 0.0, 64.0, 64.0));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn take_clips_to_bounds_and_snaps() {
        let mut d = Damage::new();
        d.add(Rect::new(-10.5, -10.5, 64.0, 64.0));
        let out = d.take(Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].x, 0.0);
        assert_eq!(out[0].y, 0.0);
    }

    #[test]
    fn adding_damage_that_is_full_marks_full() {
        let mut a = Damage::new();
        let mut b = Damage::new();
        b.mark_full();
        a.add_damage(&b);
        assert!(a.is_full());
    }
}
