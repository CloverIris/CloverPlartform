//! Layout and appearance description.
//!
//! `Style` is deliberately a plain data struct: it is what XAML attributes parse
//! into, what the declarative builders fill in, and what the layout engine reads.
//! Both authoring modes therefore land on the same representation instead of
//! needing two parallel layout paths.

use clover_core::{Align, Color, Edges, Orientation};

/// A resolved-able length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// Size to content.
    Auto,
    /// Takes an equal share of the parent's leftover space on the main axis,
    /// and stretches across the cross axis.
    Fill,
    /// Logical pixels.
    Px(f32),
    /// Fraction of the parent's available extent, `0.0..=1.0`.
    Percent(f32),
}

impl Default for Length {
    fn default() -> Self {
        Length::Auto
    }
}

impl Length {
    /// Resolves against the available extent, or `None` when it depends on
    /// content.
    pub fn resolve(self, available: f32) -> Option<f32> {
        match self {
            // Fill depends on what the siblings need, so it cannot be resolved
            // before the arrange pass.
            Length::Auto | Length::Fill => None,
            Length::Px(v) => Some(v.max(0.0)),
            Length::Percent(p) => Some((available * p.clamp(0.0, 1.0)).max(0.0)),
        }
    }

    /// Parses `"Auto"`, `"120"`, `"120px"`, `"50%"`.
    pub fn parse(text: &str) -> Option<Length> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("auto") {
            return Some(Length::Auto);
        }
        if text.eq_ignore_ascii_case("fill") || text == "*" {
            return Some(Length::Fill);
        }
        if let Some(percent) = text.strip_suffix('%') {
            return percent.trim().parse::<f32>().ok().map(|v| Length::Percent(v / 100.0));
        }
        let numeric = text.strip_suffix("px").unwrap_or(text).trim();
        numeric.parse::<f32>().ok().map(Length::Px)
    }
}

/// Mouse cursor a node asks the host to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    /// Inherit from the parent, ultimately the arrow.
    #[default]
    Default,
    Pointer,
    Text,
    ResizeHorizontal,
    ResizeVertical,
    Move,
    Crosshair,
}

/// One colour stop in a gradient, `offset` in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Color,
}

impl GradientStop {
    pub fn new(offset: f32, color: Color) -> Self {
        Self { offset: offset.clamp(0.0, 1.0), color }
    }
}

/// Where a gradient starts and ends, in normalised box coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientGeometry {
    /// `angle` in degrees: 0 points right, 90 points down.
    Linear { angle: f32 },
    /// Centre and radius as fractions of the box, `0.5,0.5` / `0.7` by default.
    Radial { center: (f32, f32), radius: f32 },
}

/// How a control's background is painted.
///
/// The solid case stays a plain colour so the common path never touches the
/// image pipeline; textures are resolved to a handle at build time, so the
/// renderer never sees a string.
#[derive(Clone, Debug, PartialEq)]
pub enum Brush {
    /// Flat colour.
    Solid(Color),
    /// An image, fitted into the background box.
    Texture {
        handle: crate::node::ImageHandle,
        fit: Fit,
        /// Multiplied into the texture's alpha.
        opacity: f32,
    },
    /// A linear or radial gradient across the background box.
    Gradient {
        geometry: GradientGeometry,
        stops: Vec<GradientStop>,
    },
}

impl Brush {
    pub fn solid(color: Color) -> Self {
        Brush::Solid(color)
    }

    /// A linear gradient at `angle` degrees (0 = right, 90 = down).
    pub fn linear(angle: f32, stops: Vec<GradientStop>) -> Self {
        Brush::Gradient { geometry: GradientGeometry::Linear { angle }, stops }
    }

    /// A radial gradient centred at `center` (0..1) with `radius` (0..1).
    pub fn radial(center: (f32, f32), radius: f32, stops: Vec<GradientStop>) -> Self {
        Brush::Gradient { geometry: GradientGeometry::Radial { center, radius }, stops }
    }
}

/// How an image is fitted into its box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fit {
    /// Fill the box, ignoring aspect ratio.
    #[default]
    Fill,
    /// Contain, preserving aspect ratio (letterboxed).
    Contain,
    /// Cover, preserving aspect ratio (cropped).
    Cover,
}

/// Interaction state that skins can react to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeState {
    pub hovered: bool,
    pub pressed: bool,
    pub focused: bool,
    pub disabled: bool,
}

/// Everything the layout engine and rasteriser need to know about a node.
#[derive(Clone, Debug)]
pub struct Style {
    pub width: Length,
    pub height: Length,
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_height: Option<f32>,

    pub margin: Edges,
    pub padding: Edges,

    /// Child flow direction for containers.
    pub direction: Orientation,
    /// When set, children are laid out in this many uniform columns instead of a
    /// single stack line.
    pub grid_columns: Option<u32>,
    /// Cursor to show while the pointer is over this node.
    pub cursor: Cursor,
    /// When false the node paints but takes no hits, so clicks pass through to
    /// whatever is underneath. Overlay wrappers rely on this.
    pub hit_test_visible: bool,
    /// Takes this node out of the flow and places it at this offset from the
    /// parent's content origin, in logical pixels.
    ///
    /// Floating panels, menus and tooltips need this: a margin can shift a node
    /// inside its slot, but it cannot make the node ignore the slot entirely.
    pub position: Option<clover_core::Point>,
    /// Main-axis gap between children.
    pub gap: f32,
    /// Cross-axis alignment of children.
    pub align: Align,
    /// Main-axis distribution of children.
    pub justify: Align,

    pub background: Color,
    /// Background texture; when set it paints over `background`.
    pub brush: Option<Brush>,
    pub radius: f32,
    pub border_width: f32,
    pub border_color: Color,

    pub font_size: f32,
    pub text_color: Color,
    /// Text alignment inside the node's content box.
    pub text_align: Align,

    pub opacity: f32,
    /// Clip children to this node's bounds.
    pub clip: bool,
    /// Break text onto multiple lines to fit the content box.
    pub wrap: bool,
    /// Behaves as a scroll viewport during measurement: never reports content
    /// taller than the space it was offered. Set by `Node::scrollable`.
    pub scroll_viewport: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            width: Length::Auto,
            height: Length::Auto,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            margin: Edges::ZERO,
            padding: Edges::ZERO,
            direction: Orientation::Vertical,
            grid_columns: None,
            cursor: Cursor::Default,
            hit_test_visible: true,
            position: None,
            gap: 0.0,
            align: Align::Stretch,
            justify: Align::Start,
            background: Color::TRANSPARENT,
            brush: None,
            radius: 0.0,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            font_size: 14.0,
            text_color: Color::rgb(0xE8, 0xEA, 0xED),
            text_align: Align::Start,
            opacity: 1.0,
            clip: false,
            wrap: false,
            scroll_viewport: false,
        }
    }
}

impl Style {
    /// Applies min/max constraints to a measured extent.
    /// See [`Style::scroll_viewport`].
    pub fn scroll_viewport(&self) -> bool {
        self.scroll_viewport
    }

    pub fn clamp_width(&self, value: f32) -> f32 {
        let mut v = value;
        if let Some(min) = self.min_width {
            v = v.max(min);
        }
        if let Some(max) = self.max_width {
            v = v.min(max);
        }
        v
    }

    pub fn clamp_height(&self, value: f32) -> f32 {
        let mut v = value;
        if let Some(min) = self.min_height {
            v = v.max(min);
        }
        if let Some(max) = self.max_height {
            v = v.min(max);
        }
        v
    }

    /// Whether this node wants to grow along the parent's main axis.
    pub fn fills_main(&self, horizontal: bool) -> bool {
        if horizontal {
            self.width == Length::Fill
        } else {
            self.height == Length::Fill
        }
    }

    /// Whether this node wants to fill the parent's cross axis.
    pub fn fills_cross(&self, horizontal: bool) -> bool {
        if horizontal {
            self.height == Length::Fill
        } else {
            self.width == Length::Fill
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lengths() {
        assert_eq!(Length::parse("Auto"), Some(Length::Auto));
        assert_eq!(Length::parse("Fill"), Some(Length::Fill));
        assert_eq!(Length::parse("*"), Some(Length::Fill));
        assert_eq!(Length::parse("120"), Some(Length::Px(120.0)));
        assert_eq!(Length::parse("120px"), Some(Length::Px(120.0)));
        assert_eq!(Length::parse("50%"), Some(Length::Percent(0.5)));
        assert_eq!(Length::parse("nope"), None);
    }

    #[test]
    fn fill_needs_arrange_to_resolve() {
        assert_eq!(Length::Fill.resolve(100.0), None);
    }

    #[test]
    fn fill_axis_classification() {
        let style = Style { width: Length::Fill, height: Length::Auto, ..Style::default() };
        assert!(style.fills_main(true), "width fill grows in a row");
        assert!(!style.fills_main(false), "width fill is a cross fill in a column");
        assert!(style.fills_cross(false));
        assert!(!style.fills_cross(true));
    }

    #[test]
    fn resolves_lengths() {
        assert_eq!(Length::Px(10.0).resolve(100.0), Some(10.0));
        assert_eq!(Length::Percent(0.25).resolve(100.0), Some(25.0));
        assert_eq!(Length::Auto.resolve(100.0), None);
    }

    #[test]
    fn clamps_respect_bounds() {
        let style = Style { min_width: Some(50.0), max_width: Some(80.0), ..Style::default() };
        assert_eq!(style.clamp_width(10.0), 50.0);
        assert_eq!(style.clamp_width(200.0), 80.0);
    }
}
