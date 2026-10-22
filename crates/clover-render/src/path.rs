//! Rasterising vector paths.
//!
//! Same rasteriser as text (`ab_glyph_rasterizer`), which is why icon edges look
//! identical to glyph edges: both are analytic coverage, not sampled bitmaps.
//!
//! Fill closes open subpaths (SVG semantics). Stroke flattens curves into
//! segments and draws a quad per segment plus a round join, which keeps corners
//! solid at any width without implementing mitre joins.

use ab_glyph_rasterizer::{point, Rasterizer};
use clover_core::{PathCmd, PathGeometry, Point};

/// An 8-bit coverage mask placed at `origin` in physical pixels.
pub struct PathMask {
    pub origin: Point,
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// Rasterises `geometry` with `scale`, placing its origin at `origin`.
///
/// `stroke_width` is in logical units; `0.0` disables the stroke pass.
pub fn rasterize(
    geometry: &PathGeometry,
    scale: f32,
    origin: Point,
    fill: bool,
    stroke_width: f32,
) -> Option<PathMask> {
    if geometry.is_empty() || scale <= 0.0 || (!fill && stroke_width <= 0.0) {
        return None;
    }
    let bounds = geometry.bounds();
    let stroke_px = (stroke_width * scale).max(0.0);
    let padding = stroke_px * 0.5 + 1.0;
    let left = origin.x + bounds.x * scale - padding;
    let top = origin.y + bounds.y * scale - padding;
    let width = (bounds.w * scale + padding * 2.0).ceil().max(1.0);
    let height = (bounds.h * scale + padding * 2.0).ceil().max(1.0);
    // A single icon larger than this is a bug, not a use case.
    if width > 4096.0 || height > 4096.0 {
        return None;
    }
    let origin_x = left.floor();
    let origin_y = top.floor();
    let mut rasterizer = Rasterizer::new(width as usize, height as usize);

    let map = |p: Point| point(p.x * scale + origin.x - origin_x, p.y * scale + origin.y - origin_y);

    if fill {
        fill_path(&mut rasterizer, geometry, &map);
    }
    if stroke_width > 0.0 {
        stroke_path(&mut rasterizer, geometry, &map, stroke_px);
    }

    let mut coverage = vec![0u8; width as usize * height as usize];
    rasterizer.for_each_pixel_2d(|x, y, alpha| {
        let index = y as usize * width as usize + x as usize;
        if index < coverage.len() {
            coverage[index] = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    });

    Some(PathMask {
        origin: Point::new(origin_x, origin_y),
        width: width as u32,
        height: height as u32,
        coverage,
    })
}

fn fill_path(rasterizer: &mut Rasterizer, geometry: &PathGeometry, map: &impl Fn(Point) -> ab_glyph_rasterizer::Point) {
    let mut current = Point::ZERO;
    let mut start = Point::ZERO;
    let mut open = false;
    let close = |rasterizer: &mut Rasterizer, current: Point, start: Point, open: bool| {
        if open {
            let a = map(current);
            let b = map(start);
            if (a.x - b.x).abs() > 0.001 || (a.y - b.y).abs() > 0.001 {
                rasterizer.draw_line(a, b);
            }
        }
    };
    for command in &geometry.commands {
        match *command {
            PathCmd::Move(point) => {
                close(rasterizer, current, start, open);
                current = point;
                start = point;
                open = true;
            }
            PathCmd::Line(point) => {
                rasterizer.draw_line(map(current), map(point));
                current = point;
            }
            PathCmd::Quad(control, end) => {
                rasterizer.draw_quad(map(current), map(control), map(end));
                current = end;
            }
            PathCmd::Cubic(first, second, end) => {
                rasterizer.draw_cubic(map(current), map(first), map(second), map(end));
                current = end;
            }
            PathCmd::Close => {
                close(rasterizer, current, start, open);
                current = start;
            }
        }
    }
    close(rasterizer, current, start, open);
}

fn stroke_path(
    rasterizer: &mut Rasterizer,
    geometry: &PathGeometry,
    map: &impl Fn(Point) -> ab_glyph_rasterizer::Point,
    width: f32,
) {
    let half = (width * 0.5).max(0.35);
    let mut subpath: Vec<Point> = Vec::new();
    let mut flush = |points: &mut Vec<Point>| {
        if points.len() >= 2 {
            for pair in points.windows(2) {
                stroke_segment(rasterizer, map(pair[0]), map(pair[1]), half);
            }
            for point in points.iter() {
                round_join(rasterizer, map(*point), half);
            }
        }
        points.clear();
    };
    for command in &geometry.commands {
        match *command {
            PathCmd::Move(point) => {
                flush(&mut subpath);
                subpath.push(point);
            }
            PathCmd::Line(point) => subpath.push(point),
            PathCmd::Quad(control, end) => {
                let from = subpath.last().copied().unwrap_or(control);
                flatten_quad(&mut subpath, from, control, end);
            }
            PathCmd::Cubic(first, second, end) => {
                let from = subpath.last().copied().unwrap_or(first);
                flatten_cubic(&mut subpath, from, first, second, end);
            }
            PathCmd::Close => flush(&mut subpath),
        }
    }
    flush(&mut subpath);
}

/// One stroke segment as a quad, extended by half the width at both ends so the
/// joins do not show gaps.
fn stroke_segment(
    rasterizer: &mut Rasterizer,
    a: ab_glyph_rasterizer::Point,
    b: ab_glyph_rasterizer::Point,
    half: f32,
) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON {
        return;
    }
    let nx = -dy / length * half;
    let ny = dx / length * half;
    let ex = dx / length * half;
    let ey = dy / length * half;
    let p0 = point(a.x - ex + nx, a.y - ey + ny);
    let p1 = point(b.x + ex + nx, b.y + ey + ny);
    let p2 = point(b.x + ex - nx, b.y + ey - ny);
    let p3 = point(a.x - ex - nx, a.y - ey - ny);
    rasterizer.draw_line(p0, p1);
    rasterizer.draw_line(p1, p2);
    rasterizer.draw_line(p2, p3);
    rasterizer.draw_line(p3, p0);
}

/// A circular join, approximated with a twelve-sided polygon.
fn round_join(rasterizer: &mut Rasterizer, centre: ab_glyph_rasterizer::Point, radius: f32) {
    const SIDES: usize = 12;
    let mut previous = None;
    for side in 0..=SIDES {
        let angle = side as f32 / SIDES as f32 * std::f32::consts::TAU;
        let current = point(centre.x + angle.cos() * radius, centre.y + angle.sin() * radius);
        if let Some(previous) = previous {
            rasterizer.draw_line(previous, current);
        }
        previous = Some(current);
    }
}

fn flatten_quad(out: &mut Vec<Point>, start: Point, control: Point, end: Point) {
    const STEPS: usize = 12;
    for step in 1..=STEPS {
        let t = step as f32 / STEPS as f32;
        let inv = 1.0 - t;
        out.push(Point::new(
            inv * inv * start.x + 2.0 * inv * t * control.x + t * t * end.x,
            inv * inv * start.y + 2.0 * inv * t * control.y + t * t * end.y,
        ));
    }
}

fn flatten_cubic(out: &mut Vec<Point>, start: Point, first: Point, second: Point, end: Point) {
    const STEPS: usize = 16;
    for step in 1..=STEPS {
        let t = step as f32 / STEPS as f32;
        let inv = 1.0 - t;
        let a = inv * inv * inv;
        let b = 3.0 * inv * inv * t;
        let c = 3.0 * inv * t * t;
        let d = t * t * t;
        out.push(Point::new(
            a * start.x + b * first.x + c * second.x + d * end.x,
            a * start.y + b * first.y + c * second.y + d * end.y,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle() -> PathGeometry {
        PathGeometry::from_svg("M0 0 L16 0 L8 16 Z").unwrap()
    }

    #[test]
    fn fills_the_inside_of_a_triangle() {
        let mask = rasterize(&triangle(), 1.0, Point::ZERO, true, 0.0).unwrap();
        let at = |x: u32, y: u32| mask.coverage[(y * mask.width + x) as usize];
        // A point well inside the triangle is opaque, a corner is not.
        assert!(at(8, 6) > 200, "centre should be filled");
        assert!(at(0, 15) < 20, "the empty corner stays clear");
    }

    #[test]
    fn antialiasing_produces_partial_coverage() {
        let mask = rasterize(&triangle(), 1.0, Point::ZERO, true, 0.0).unwrap();
        assert!(
            mask.coverage.iter().any(|value| *value > 20 && *value < 235),
            "an edge should have partial coverage"
        );
    }

    #[test]
    fn scale_changes_the_mask_size() {
        let small = rasterize(&triangle(), 1.0, Point::ZERO, true, 0.0).unwrap();
        let large = rasterize(&triangle(), 2.0, Point::ZERO, true, 0.0).unwrap();
        assert!(large.width > small.width * 3 / 2, "2x must be bigger than 1x");
    }

    #[test]
    fn stroke_draws_around_the_outline() {
        let open = PathGeometry::from_svg("M0 8 L16 8").unwrap();
        let mask = rasterize(&open, 1.0, Point::ZERO, false, 2.0).unwrap();
        let at = |x: u32, y: u32| mask.coverage[(y * mask.width + x) as usize];
        let middle_y = mask.height / 2;
        assert!(at(8, middle_y) > 200, "the stroke covers the line");
        assert!(at(8, 0) < 20, "and does not fill the whole mask");
    }

    #[test]
    fn empty_or_invisible_paths_produce_nothing() {
        let empty = PathGeometry::default();
        assert!(rasterize(&empty, 1.0, Point::ZERO, true, 0.0).is_none());
        assert!(rasterize(&triangle(), 1.0, Point::ZERO, false, 0.0).is_none());
    }

    #[test]
    fn origin_offsets_the_mask() {
        let mask = rasterize(&triangle(), 1.0, Point::new(100.0, 50.0), true, 0.0).unwrap();
        assert!(mask.origin.x >= 98.0 && mask.origin.x <= 100.0);
        assert!(mask.origin.y >= 48.0 && mask.origin.y <= 50.0);
    }
}
