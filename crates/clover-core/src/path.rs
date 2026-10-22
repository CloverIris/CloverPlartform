//! Vector paths and SVG path-data parsing.
//!
//! Icons need real curves: a rasterised PNG of an icon is wrong at every scale
//! except the one it was exported at, and the toolbar is drawn at 1x and 2x.
//! Paths are parsed once into a flat command list and rasterised at whatever
//! scale a frame needs.

use crate::{Point, Rect};

/// One path command. Coordinates are in the path's own space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathCmd {
    Move(Point),
    Line(Point),
    Quad(Point, Point),
    Cubic(Point, Point, Point),
    Close,
}

/// A path and its cached bounds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathGeometry {
    pub commands: Vec<PathCmd>,
    bounds: Rect,
}

impl PathGeometry {
    pub fn new(commands: Vec<PathCmd>) -> Self {
        let mut geometry = Self { commands, bounds: Rect::ZERO };
        geometry.bounds = measure(&geometry.commands);
        geometry
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Parses SVG path data: `M L H V C S Q T Z`, absolute and relative.
    ///
    /// Arcs (`A`) are not supported; icons here are built from curves.
    pub fn from_svg(data: &str) -> Option<Self> {
        let mut tokens = Tokenizer::new(data);
        let mut commands = Vec::new();
        let mut current = Point::ZERO;
        let mut start = Point::ZERO;
        let mut last_control: Option<Point> = None;
        let mut last_quad: Option<Point> = None;
        let mut verb = ' ';

        loop {
            tokens.skip_separators();
            let Some(next) = tokens.peek() else { break };
            if next.is_ascii_alphabetic() {
                verb = tokens.next_char()?;
            } else if verb == ' ' {
                return None;
            }
            let relative = verb.is_ascii_lowercase();
            let absolute = |point: Point| {
                if relative {
                    Point::new(current.x + point.x, current.y + point.y)
                } else {
                    point
                }
            };
            match verb.to_ascii_uppercase() {
                'M' => {
                    let point = absolute(Point::new(tokens.number()?, tokens.number()?));
                    commands.push(PathCmd::Move(point));
                    current = point;
                    start = point;
                    last_control = None;
                    last_quad = None;
                    // Subsequent pairs are implicit line-tos.
                    verb = if relative { 'l' } else { 'L' };
                }
                'L' => {
                    let point = absolute(Point::new(tokens.number()?, tokens.number()?));
                    commands.push(PathCmd::Line(point));
                    current = point;
                    last_control = None;
                    last_quad = None;
                }
                'H' => {
                    let x = tokens.number()?;
                    let point = if relative { Point::new(current.x + x, current.y) } else { Point::new(x, current.y) };
                    commands.push(PathCmd::Line(point));
                    current = point;
                }
                'V' => {
                    let y = tokens.number()?;
                    let point = if relative { Point::new(current.x, current.y + y) } else { Point::new(current.x, y) };
                    commands.push(PathCmd::Line(point));
                    current = point;
                }
                'C' => {
                    let c1 = absolute(Point::new(tokens.number()?, tokens.number()?));
                    let c2 = absolute(Point::new(tokens.number()?, tokens.number()?));
                    let end = absolute(Point::new(tokens.number()?, tokens.number()?));
                    commands.push(PathCmd::Cubic(c1, c2, end));
                    current = end;
                    last_control = Some(c2);
                    last_quad = None;
                }
                'S' => {
                    let c1 = match last_control {
                        Some(previous) => Point::new(2.0 * current.x - previous.x, 2.0 * current.y - previous.y),
                        None => current,
                    };
                    let c2 = absolute(Point::new(tokens.number()?, tokens.number()?));
                    let end = absolute(Point::new(tokens.number()?, tokens.number()?));
                    commands.push(PathCmd::Cubic(c1, c2, end));
                    current = end;
                    last_control = Some(c2);
                    last_quad = None;
                }
                'Q' => {
                    let control = absolute(Point::new(tokens.number()?, tokens.number()?));
                    let end = absolute(Point::new(tokens.number()?, tokens.number()?));
                    commands.push(PathCmd::Quad(control, end));
                    current = end;
                    last_quad = Some(control);
                    last_control = None;
                }
                'T' => {
                    let control = match last_quad {
                        Some(previous) => Point::new(2.0 * current.x - previous.x, 2.0 * current.y - previous.y),
                        None => current,
                    };
                    let end = absolute(Point::new(tokens.number()?, tokens.number()?));
                    commands.push(PathCmd::Quad(control, end));
                    current = end;
                    last_quad = Some(control);
                    last_control = None;
                }
                'Z' => {
                    commands.push(PathCmd::Close);
                    current = start;
                }
                _ => return None,
            }
        }

        if commands.is_empty() {
            return None;
        }
        Some(Self::new(commands))
    }
}

/// Bounds of the control polygon, which always contains the curve.
fn measure(commands: &[PathCmd]) -> Rect {
    let mut min = Point::new(f32::MAX, f32::MAX);
    let mut max = Point::new(f32::MIN, f32::MIN);
    let mut add = |point: Point| {
        min.x = min.x.min(point.x);
        min.y = min.y.min(point.y);
        max.x = max.x.max(point.x);
        max.y = max.y.max(point.y);
    };
    let mut current = Point::ZERO;
    for command in commands {
        match *command {
            PathCmd::Move(point) | PathCmd::Line(point) => {
                add(point);
                current = point;
            }
            PathCmd::Quad(control, end) => {
                add(control);
                add(end);
                current = end;
            }
            PathCmd::Cubic(first, second, end) => {
                add(first);
                add(second);
                add(end);
                current = end;
            }
            PathCmd::Close => add(current),
        }
    }
    if min.x > max.x {
        return Rect::ZERO;
    }
    Rect::new(min.x, min.y, max.x - min.x, max.y - min.y)
}

struct Tokenizer<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(data: &'a str) -> Self {
        Self { bytes: data.as_bytes(), index: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.bytes.get(self.index).map(|byte| *byte as char)
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.index += 1;
        Some(ch)
    }

    fn skip_separators(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() || ch == ',' {
                self.index += 1;
            } else {
                break;
            }
        }
    }

    fn number(&mut self) -> Option<f32> {
        self.skip_separators();
        let start = self.index;
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.index += 1;
        }
        while matches!(self.peek(), Some(ch) if ch.is_ascii_digit() || ch == '.') {
            self.index += 1;
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.index += 1;
            if matches!(self.peek(), Some('-') | Some('+')) {
                self.index += 1;
            }
            while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
                self.index += 1;
            }
        }
        std::str::from_utf8(&self.bytes[start..self.index]).ok()?.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_triangle() {
        let path = PathGeometry::from_svg("M0 0 L10 0 L5 8 Z").unwrap();
        assert_eq!(path.commands.len(), 4);
        assert!(matches!(path.commands[3], PathCmd::Close));
    }

    #[test]
    fn parses_relative_commands() {
        let path = PathGeometry::from_svg("m10 10 l5 0 l0 5 z").unwrap();
        assert_eq!(path.commands[0], PathCmd::Move(Point::new(10.0, 10.0)));
        assert_eq!(path.commands[1], PathCmd::Line(Point::new(15.0, 10.0)));
        assert_eq!(path.commands[2], PathCmd::Line(Point::new(15.0, 15.0)));
    }

    #[test]
    fn parses_horizontal_and_vertical() {
        let path = PathGeometry::from_svg("M0 0 H10 V10 H0 Z").unwrap();
        assert_eq!(path.commands[1], PathCmd::Line(Point::new(10.0, 0.0)));
        assert_eq!(path.commands[2], PathCmd::Line(Point::new(10.0, 10.0)));
    }

    #[test]
    fn parses_curves_and_smooth_reflection() {
        let path = PathGeometry::from_svg("M0 0 C1 1 2 2 3 3 S5 5 6 6").unwrap();
        assert!(matches!(path.commands[1], PathCmd::Cubic(..)));
        match path.commands[2] {
            PathCmd::Cubic(first, _, _) => {
                // Reflection of (2,2) about (3,3) is (4,4).
                assert_eq!(first, Point::new(4.0, 4.0));
            }
            other => panic!("expected a cubic, got {other:?}"),
        }
    }

    #[test]
    fn parses_quadratic() {
        let path = PathGeometry::from_svg("M0 0 Q5 10 10 0").unwrap();
        assert!(matches!(path.commands[1], PathCmd::Quad(..)));
    }

    #[test]
    fn implicit_lineto_after_move() {
        let path = PathGeometry::from_svg("M0 0 10 0 10 10").unwrap();
        assert_eq!(path.commands.len(), 3);
        assert_eq!(path.commands[1], PathCmd::Line(Point::new(10.0, 0.0)));
    }

    #[test]
    fn bounds_cover_the_path() {
        let path = PathGeometry::from_svg("M2 3 L12 3 L12 9 Z").unwrap();
        assert_eq!(path.bounds(), Rect::new(2.0, 3.0, 10.0, 6.0));
    }

    #[test]
    fn rejects_garbage() {
        assert!(PathGeometry::from_svg("").is_none());
        assert!(PathGeometry::from_svg("A0 0").is_none());
        assert!(PathGeometry::from_svg("M").is_none());
    }
}
