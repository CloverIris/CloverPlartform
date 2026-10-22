//! Single-pass box layout.
//!
//! Every node stores **absolute** bounds in surface space. That keeps hit
//! testing, damage and the display-list builder trivial (no ancestor transform
//! accumulation), at the cost of one extra add during arrange.
//!
//! The algorithm is a small flexbox: measure content, resolve explicit lengths
//! against the parent, then distribute children along the main axis with
//! `justify` and place them on the cross axis with `align`.

use clover_core::{Align, Point, Rect, Size};

use crate::node::{ImageHandle, Node, View};
use crate::style::Length;

/// Intrinsic measurements the layout engine needs but does not own.
pub trait LayoutMetrics {
    /// Size of a single line of text at `font_px`.
    fn measure_text(&self, text: &str, font_px: f32) -> Size;
    /// Line height for `font_px`.
    fn line_height(&self, font_px: f32) -> f32;
    /// Natural size of a registered image.
    fn measure_image(&self, handle: ImageHandle) -> Size;
}

/// A metrics provider that treats text and images as empty, for tests.
pub struct EmptyMetrics;

impl LayoutMetrics for EmptyMetrics {
    fn measure_text(&self, _text: &str, _font_px: f32) -> Size {
        Size::ZERO
    }
    fn line_height(&self, font_px: f32) -> f32 {
        font_px * 1.2
    }
    fn measure_image(&self, _handle: ImageHandle) -> Size {
        Size::ZERO
    }
}

/// Lays out `root` inside `viewport`, assigning absolute bounds.
pub fn layout(root: &mut Node, viewport: Size, metrics: &dyn LayoutMetrics) {
    if viewport.is_empty() {
        return;
    }
    root.bounds = Rect::from_size(viewport);
    arrange(root, root.bounds, metrics);
}

/// Measures a node's border-box size for the given available content extent.
pub fn measure(node: &Node, available: Size, metrics: &dyn LayoutMetrics) -> Size {
    if !node.visible {
        return Size::ZERO;
    }
    let style = &node.style;
    let padding = style.padding;

    let explicit_w = style.width.resolve(available.w);
    let explicit_h = style.height.resolve(available.h);

    let child_available = Size::new(
        explicit_w.map(|w| (w - padding.horizontal()).max(0.0)).unwrap_or(available.w),
        explicit_h.map(|h| (h - padding.vertical()).max(0.0)).unwrap_or(available.h),
    );

    let mut content = Size::ZERO;
    let horizontal = style.direction.is_horizontal();

    let mut visible_children = node.children.iter().filter(|c| c.visible).peekable();
    let mut index = 0usize;
    while let Some(child) = visible_children.next() {
        let margin = child.style.margin;
        let child_available = Size::new(
            (child_available.w - margin.horizontal()).max(0.0),
            (child_available.h - margin.vertical()).max(0.0),
        );
        let child_size = measure(child, child_available, metrics);
        let outer_w = child_size.w + margin.horizontal();
        let outer_h = child_size.h + margin.vertical();
        let gap = if index > 0 { style.gap } else { 0.0 };
        if horizontal {
            content.w += outer_w + gap;
            content.h = content.h.max(outer_h);
        } else {
            content.h += outer_h + gap;
            content.w = content.w.max(outer_w);
        }
        index += 1;
    }

    match &node.view {
        View::Text(text) => {
            let text_size = metrics.measure_text(text, style.font_size);
            content.w = content.w.max(text_size.w);
            content.h = content.h.max(metrics.line_height(style.font_size));
        }
        View::Image { handle, .. } => {
            let natural = metrics.measure_image(*handle);
            content.w = content.w.max(natural.w);
            content.h = content.h.max(natural.h);
        }
        View::Surface { .. } | View::Empty => {}
    }

    let width = explicit_w
        .unwrap_or(content.w + padding.horizontal())
        .max(0.0);
    let height = explicit_h
        .unwrap_or(content.h + padding.vertical())
        .max(0.0);

    Size::new(style.clamp_width(width), style.clamp_height(height))
}

fn arrange(node: &mut Node, border_box: Rect, metrics: &dyn LayoutMetrics) {
    node.bounds = border_box;
    if !node.visible {
        return;
    }
    if node.children.is_empty() {
        return;
    }

    let content_box = node.style.padding.deflate(border_box);
    let content_size = content_box.size();
    let horizontal = node.style.direction.is_horizontal();
    let gap = node.style.gap;

    // Measure children against the resolved content box.
    let measured: Vec<Size> = node
        .children
        .iter()
        .map(|child| {
            if !child.visible {
                return Size::ZERO;
            }
            let margin = child.style.margin;
            let available = Size::new(
                (content_size.w - margin.horizontal()).max(0.0),
                (content_size.h - margin.vertical()).max(0.0),
            );
            measure(child, available, metrics)
        })
        .collect();

    // Total main-axis extent including margins and gaps.
    let mut total_main = 0.0f32;
    let mut visible_count = 0usize;
    for (child, size) in node.children.iter().zip(&measured) {
        if !child.visible {
            continue;
        }
        let margin = child.style.margin;
        total_main += if horizontal { size.w + margin.horizontal() } else { size.h + margin.vertical() };
        visible_count += 1;
    }
    if visible_count > 1 {
        total_main += gap * (visible_count - 1) as f32;
    }

    let main_available = if horizontal { content_size.w } else { content_size.h };
    let slack = (main_available - total_main).max(0.0);
    let mut cursor = match node.style.justify {
        Align::Start | Align::Stretch => 0.0,
        Align::Center => slack * 0.5,
        Align::End => slack,
    };
    let gap_extra = if matches!(node.style.justify, Align::Stretch) && visible_count > 1 {
        slack / (visible_count - 1) as f32
    } else {
        0.0
    };

    for (child, size) in node.children.iter_mut().zip(measured) {
        if !child.visible {
            child.bounds = Rect::ZERO;
            continue;
        }
        let margin = child.style.margin;
        let cross_available = if horizontal { content_size.h } else { content_size.w };
        let cross_margin = if horizontal { margin.vertical() } else { margin.horizontal() };
        let cross_space = (cross_available - cross_margin).max(0.0);

        // Stretch only fills the cross axis when the child did not ask for an
        // explicit size there; an explicit Width/Height always wins, matching
        // WPF's precedence rules.
        let cross_is_auto = if horizontal {
            child.style.height == Length::Auto
        } else {
            child.style.width == Length::Auto
        };
        let (child_w, child_h) = if node.style.align == Align::Stretch && cross_is_auto {
            if horizontal {
                (size.w, cross_space)
            } else {
                (cross_space, size.h)
            }
        } else {
            (size.w, size.h)
        };

        let cross_extent = if horizontal { child_h + cross_margin } else { child_w + cross_margin };
        let cross_offset = node.style.align.offset(cross_available, cross_extent);

        let (x, y) = if horizontal {
            (
                content_box.x + cursor + margin.left,
                content_box.y + cross_offset + margin.top,
            )
        } else {
            (
                content_box.x + cross_offset + margin.left,
                content_box.y + cursor + margin.top,
            )
        };

        let child_rect = Rect::new(x, y, child_w, child_h);
        arrange(child, child_rect, metrics);
        cursor += if horizontal {
            child_w + margin.horizontal() + gap + gap_extra
        } else {
            child_h + margin.vertical() + gap + gap_extra
        };
    }
}

/// Convenience: the content box of a node, in surface space.
pub fn content_rect(node: &Node) -> Rect {
    node.style.padding.deflate(node.bounds)
}

/// Convenience: deepest node whose absolute bounds contain `point`.
pub fn hit_test<'a>(node: &'a Node, point: Point) -> Option<&'a Node> {
    if !node.visible || !node.bounds.contains(point) {
        return None;
    }
    for child in node.children.iter().rev() {
        if let Some(found) = hit_test(child, point) {
            return Some(found);
        }
    }
    Some(node)
}

/// Keeps `Length` in scope for downstream users of this module.
pub fn resolve(length: Length, available: f32) -> Option<f32> {
    length.resolve(available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node;
    use clover_core::{Edges, Orientation};

    fn size(w: f32, h: f32) -> Size {
        Size::new(w, h)
    }

    #[test]
    fn vertical_stack_sums_children() {
        let mut root = node::column()
            .child(node::panel().size(Length::Px(50.0), Length::Px(10.0)))
            .child(node::panel().size(Length::Px(30.0), Length::Px(20.0)))
            .gap(4.0);
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.bounds, Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(root.children[0].bounds, Rect::new(0.0, 0.0, 50.0, 10.0));
        assert_eq!(root.children[1].bounds, Rect::new(0.0, 14.0, 30.0, 20.0));
    }

    #[test]
    fn horizontal_stack_sums_widths() {
        let mut root = node::row()
            .child(node::panel().size(Length::Px(20.0), Length::Px(10.0)))
            .child(node::panel().size(Length::Px(20.0), Length::Px(10.0)))
            .gap(6.0);
        layout(&mut root, size(100.0, 40.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.x, 0.0);
        assert_eq!(root.children[1].bounds.x, 26.0);
    }

    #[test]
    fn padding_offsets_children() {
        let mut root = node::column()
            .padding(Edges::all(8.0))
            .child(node::panel().size(Length::Px(10.0), Length::Px(10.0)));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.x, 8.0);
        assert_eq!(root.children[0].bounds.y, 8.0);
    }

    #[test]
    fn center_justify_centers_main_axis() {
        let mut root = node::column()
            .justify(Align::Center)
            .child(node::panel().size(Length::Px(10.0), Length::Px(10.0)));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.y, 45.0);
    }

    #[test]
    fn stretch_fills_cross_axis() {
        let mut root = node::column()
            .align(Align::Stretch)
            .child(node::panel().size(Length::Auto, Length::Px(10.0)));
        layout(&mut root, size(80.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.w, 80.0);
    }

    #[test]
    fn percent_length_resolves_against_parent() {
        let mut root = node::column()
            .child(node::panel().size(Length::Percent(0.5), Length::Px(10.0)));
        layout(&mut root, size(200.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.w, 100.0);
    }

    #[test]
    fn hidden_children_do_not_consume_space() {
        let mut root = node::column()
            .child(node::panel().size(Length::Px(10.0), Length::Px(10.0)))
            .child(node::panel().size(Length::Px(10.0), Length::Px(10.0)).visible(false))
            .child(node::panel().size(Length::Px(10.0), Length::Px(10.0)));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[2].bounds.y, 10.0);
    }

    #[test]
    fn margin_shifts_child_position() {
        let mut root = node::row().child(
            node::panel()
                .size(Length::Px(10.0), Length::Px(10.0))
                .margin(Edges::new(5.0, 2.0, 0.0, 0.0)),
        );
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.x, 5.0);
        assert_eq!(root.children[0].bounds.y, 2.0);
    }

    #[test]
    fn hit_test_returns_deepest() {
        let mut root = node::column()
            .padding(Edges::all(10.0))
            .child(node::panel().size(Length::Px(50.0), Length::Px(50.0)).name("inner"));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        let hit = hit_test(&root, Point::new(20.0, 20.0)).unwrap();
        assert_eq!(hit.name.as_deref(), Some("inner"));
        let miss = hit_test(&root, Point::new(95.0, 95.0)).unwrap();
        assert_eq!(miss.id, root.id);
    }

    #[test]
    fn main_axis_direction_is_honoured() {
        assert!(Orientation::Horizontal.is_horizontal());
        assert!(!Orientation::Vertical.is_horizontal());
    }
}
