//! Single-pass box layout with main-axis flex distribution.
//!
//! Every node stores **absolute** bounds in surface space. That keeps hit
//! testing, damage and the display-list builder trivial (no ancestor transform
//! accumulation), at the cost of one extra add during arrange.
//!
//! The algorithm is a small flexbox:
//!
//! 1. measure every child against the content box,
//! 2. give the parent's leftover main-axis space to `Length::Fill` children,
//! 3. distribute the remaining slack with `justify`,
//! 4. place each child on the cross axis with `align` / `Length::Fill`.
//!
//! Explicit sizes always win over stretch.

use clover_core::{Align, Edges, Point, Rect, Size};

use crate::node::{ImageHandle, Node, View};
use crate::style::Length;

/// Intrinsic measurements the layout engine needs but does not own.
pub trait LayoutMetrics {
    /// Size of a block of text at `font_px`.
    ///
    /// When `max_width` is `Some`, the text is wrapped to that width and the
    /// returned height covers every line.
    fn measure_text(&self, text: &str, font_px: f32, max_width: Option<f32>) -> Size;
    /// Line height for `font_px`.
    fn line_height(&self, font_px: f32) -> f32;
    /// Natural size of a registered image.
    fn measure_image(&self, handle: ImageHandle) -> Size;
}

/// A metrics provider that treats text and images as empty, for tests.
pub struct EmptyMetrics;

impl LayoutMetrics for EmptyMetrics {
    fn measure_text(&self, _text: &str, _font_px: f32, _max_width: Option<f32>) -> Size {
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

    let inner_width = explicit_w
        .map(|w| (w - padding.horizontal()).max(0.0))
        .unwrap_or((available.w - padding.horizontal()).max(0.0));

    let child_available = Size::new(
        inner_width,
        explicit_h.map(|h| (h - padding.vertical()).max(0.0)).unwrap_or(available.h),
    );

    if let Some(widget) = &node.widget {
        let natural = match widget {
            crate::node::Widget::Slider { show_value, .. } => {
                Size::new(if *show_value { 140.0 } else { 100.0 }, 20.0)
            }
            crate::node::Widget::CheckBox { .. } => Size::new(14.0, 14.0),
            crate::node::Widget::ComboBox { .. } => Size::new(120.0, 20.0),
            crate::node::Widget::Splitter { horizontal, .. } => {
                if *horizontal {
                    Size::new(4.0, 100.0)
                } else {
                    Size::new(100.0, 4.0)
                }
            }
            crate::node::Widget::ScrollBar { horizontal, .. } => {
                if *horizontal {
                    Size::new(100.0, 10.0)
                } else {
                    Size::new(10.0, 100.0)
                }
            }
        };
        let width = style.clamp_width(explicit_w.unwrap_or(natural.w));
        let height = style.clamp_height(explicit_h.unwrap_or(natural.h));
        return Size::new(width, height);
    }

    if let Some(columns) = style.grid_columns {
        return measure_grid(node, available, metrics, columns.max(1) as usize);
    }

    let mut content = Size::ZERO;
    let horizontal = style.direction.is_horizontal();

    let mut index = 0usize;
    for child in node.children.iter().filter(|c| c.visible) {
        let margin = child.style.margin;
        let available_for_child = Size::new(
            (child_available.w - margin.horizontal()).max(0.0),
            (child_available.h - margin.vertical()).max(0.0),
        );
        let child_size = measure(child, available_for_child, metrics);
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
            let max_width = if style.wrap && inner_width > 0.0 { Some(inner_width) } else { None };
            let text_size = metrics.measure_text(text, style.font_size, max_width);
            let capped = if max_width.is_some() { text_size.w.min(inner_width) } else { text_size.w };
            content.w = content.w.max(capped);
            content.h = content.h.max(text_size.h);
        }
        View::Path { geometry, stroke_width, .. } => {
            let bounds = geometry.bounds();
            let pad = stroke_width * 0.5;
            let natural = Size::new(bounds.w + pad * 2.0, bounds.h + pad * 2.0);
            content.w = content.w.max(natural.w);
            content.h = content.h.max(natural.h);
        }
        View::TextInput { text, placeholder, caret, composition } => {
            let (displayed, _) = crate::node::text_input_display(text, composition.as_ref(), *caret);
            let shown = if displayed.is_empty() { placeholder } else { &displayed };
            let max_width = if inner_width > 0.0 { Some(inner_width) } else { None };
            let size = metrics.measure_text(shown, style.font_size, max_width);
            content.w = content.w.max(size.w);
            content.h = content.h.max(metrics.line_height(style.font_size));
        }
        View::Image { handle, .. } => {
            let natural = metrics.measure_image(*handle);
            content.w = content.w.max(natural.w);
            content.h = content.h.max(natural.h);
        }
        View::Surface { .. } | View::Empty => {}
    }

    // A scroll container is a viewport: it must never ask to be taller than the
    // space it was offered, or its content inflates the ancestors and `Fill`
    // resolves to the content height - a feedback loop that makes scrolling
    // impossible.
    if style.scroll_viewport() && !horizontal {
        let cap = (available.h - padding.vertical()).max(0.0);
        content.h = content.h.min(cap);
    }

    let width = explicit_w.unwrap_or(content.w + padding.horizontal()).max(0.0);
    let height = explicit_h.unwrap_or(content.h + padding.vertical()).max(0.0);

    Size::new(style.clamp_width(width), style.clamp_height(height))
}

fn measure_grid(node: &Node, available: Size, metrics: &dyn LayoutMetrics, columns: usize) -> Size {
    let style = &node.style;
    let padding = style.padding;
    let explicit_w = style.width.resolve(available.w);
    let explicit_h = style.height.resolve(available.h);
    let inner_w = explicit_w
        .map(|w| (w - padding.horizontal()).max(0.0))
        .unwrap_or((available.w - padding.horizontal()).max(0.0));
    let cell_w = (inner_w / columns as f32).max(0.0);

    let mut row_heights: Vec<f32> = Vec::new();
    let mut widest = 0.0f32;
    for (index, child) in node.children.iter().filter(|c| c.visible).enumerate() {
        let margin = child.style.margin;
        let cell_avail = Size::new((cell_w - margin.horizontal()).max(0.0), available.h);
        let size = measure(child, cell_avail, metrics);
        widest = widest.max(size.w + margin.horizontal());
        let row = index / columns;
        if row >= row_heights.len() {
            row_heights.push(0.0);
        }
        row_heights[row] = row_heights[row].max(size.h + margin.vertical());
    }
    let content_h: f32 = row_heights.iter().sum();
    let width = explicit_w.unwrap_or((cell_w * columns as f32).min(widest.max(cell_w * columns as f32)))
        .max(0.0);
    let height = explicit_h.unwrap_or(content_h + padding.vertical()).max(0.0);
    Size::new(style.clamp_width(width), style.clamp_height(height))
}

fn resolve_absolute(length: Length, available: f32, fallback: f32) -> f32 {
    match length {
        Length::Auto => fallback,
        Length::Fill => available,
        Length::Percent(value) => available * value,
        Length::Px(value) => value,
    }
}

fn arrange_grid(node: &mut Node, content_box: Rect, metrics: &dyn LayoutMetrics, columns: usize) {
    let content = content_box.size();
    let cell_w = (content.w / columns as f32).max(0.0);
    let indices: Vec<usize> = node
        .children
        .iter()
        .enumerate()
        .filter(|(_, child)| child.visible && child.style.position.is_none())
        .map(|(index, _)| index)
        .collect();
    if indices.is_empty() {
        return;
    }

    let rows = (indices.len() + columns - 1) / columns;
    let mut row_heights = vec![0.0f32; rows];
    for (slot, index) in indices.iter().enumerate() {
        let margin = node.children[*index].style.margin;
        let measured = measure(
            &node.children[*index],
            Size::new((cell_w - margin.horizontal()).max(0.0), content.h),
            metrics,
        );
        let row = slot / columns;
        row_heights[row] = row_heights[row].max(measured.h + margin.vertical());
    }

    // Rows fill a container that asked for a definite height; a content-sized
    // grid keeps its measured rows.
    let natural: f32 = row_heights.iter().sum();
    if node.style.height != crate::style::Length::Auto && content.h > 0.5 {
        if natural > 0.5 {
            let stretch = content.h / natural;
            for height in row_heights.iter_mut() {
                *height *= stretch;
            }
        } else {
            let even = content.h / rows as f32;
            for height in row_heights.iter_mut() {
                *height = even;
            }
        }
    }

    let mut y = content_box.y;
    for (row, row_height) in row_heights.iter().enumerate() {
        let height = *row_height;
        for column in 0..columns {
            let slot = row * columns + column;
            if slot >= indices.len() {
                break;
            }
            let index = indices[slot];
            let child = &mut node.children[index];
            let margin = child.style.margin;
            let x = content_box.x + cell_w * column as f32;
            arrange(
                child,
                Rect::new(
                    x + margin.left,
                    y + margin.top,
                    (cell_w - margin.horizontal()).max(0.0),
                    (height - margin.vertical()).max(0.0),
                ),
                metrics,
            );
        }
        y += height;
    }
}

fn arrange(node: &mut Node, border_box: Rect, metrics: &dyn LayoutMetrics) {
    node.bounds = border_box;
    if !node.visible || node.children.is_empty() {
        return;
    }

    let content_box = node.style.padding.deflate(border_box);

    // Absolutely positioned children sit outside the flow: place them at their
    // own offset and let them take no space from their siblings.
    let mut absolute: Vec<usize> = Vec::new();
    for (index, child) in node.children.iter().enumerate() {
        if child.visible && child.style.position.is_some() {
            absolute.push(index);
        }
    }
    for index in absolute {
        let position = node.children[index].style.position.unwrap_or_default();
        let available = content_box.size();
        let measured = measure(&node.children[index], available, metrics);
        // An absolute child has no slot to fill, so `Fill` means "the whole
        // content box" rather than "whatever the flow left over".
        let width = resolve_absolute(node.children[index].style.width, available.w, measured.w);
        let height = resolve_absolute(node.children[index].style.height, available.h, measured.h);
        let child = &mut node.children[index];
        arrange(
            child,
            Rect::new(content_box.x + position.x, content_box.y + position.y, width, height),
            metrics,
        );
    }

    if let Some(columns) = node.style.grid_columns {
        arrange_grid(node, content_box, metrics, columns.max(1) as usize);
        return;
    }
    let content_size = content_box.size();
    let horizontal = node.style.direction.is_horizontal();
    let gap = node.style.gap;

    // 1. Measure every child against the content box.
    let mut sizes: Vec<Size> = Vec::with_capacity(node.children.len());
    for child in &node.children {
        if !child.visible || child.style.position.is_some() {
            sizes.push(Size::ZERO);
            continue;
        }
        let margin = child.style.margin;
        let available = Size::new(
            (content_size.w - margin.horizontal()).max(0.0),
            (content_size.h - margin.vertical()).max(0.0),
        );
        sizes.push(measure(child, available, metrics));
    }

    // 2. Split children into fixed and fill, then share the leftover.
    let mut fixed_total = 0.0f32;
    let mut fill_count = 0usize;
    let mut visible_count = 0usize;
    for (child, size) in node.children.iter().zip(&sizes) {
        if !child.visible || child.style.position.is_some() {
            continue;
        }
        visible_count += 1;
        let main_margin = main_margin_of(child.style.margin, horizontal);
        if child.style.fills_main(horizontal) {
            fill_count += 1;
        } else {
            fixed_total += main_of(*size, horizontal) + main_margin;
        }
    }
    if visible_count > 1 {
        fixed_total += gap * (visible_count - 1) as f32;
    }

    let main_available = if horizontal { content_size.w } else { content_size.h };
    if fill_count > 0 {
        let per_fill = ((main_available - fixed_total).max(0.0)) / fill_count as f32;
        for (index, child) in node.children.iter().enumerate() {
            if !child.visible || child.style.position.is_some() || !child.style.fills_main(horizontal) {
                continue;
            }
            let extent = (per_fill - main_margin_of(child.style.margin, horizontal)).max(0.0);
            if horizontal {
                sizes[index].w = extent;
            } else {
                sizes[index].h = extent;
            }
        }
    }

    // 3. Remaining slack goes to justify (zero once fill children took it).
    let mut main_total = 0.0f32;
    for (child, size) in node.children.iter().zip(&sizes) {
        if !child.visible || child.style.position.is_some() {
            continue;
        }
        main_total += main_of(*size, horizontal) + main_margin_of(child.style.margin, horizontal);
    }
    if visible_count > 1 {
        main_total += gap * (visible_count - 1) as f32;
    }
    let slack = (main_available - main_total).max(0.0);
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

    // 3b. Scrolling: measure the content, clamp the offset, and lay children out
    // in a box that is taller than the viewport and shifted by the offset.
    let mut layout_origin = content_box.origin();
    if node.scroll.enabled && !horizontal {
        let mut total = 0.0f32;
        let mut counted = 0usize;
        for (child, size) in node.children.iter().zip(&sizes) {
            if !child.visible || child.style.position.is_some() {
                continue;
            }
            total += size.h + child.style.margin.vertical();
            counted += 1;
        }
        if counted > 1 {
            total += gap * (counted - 1) as f32;
        }
        node.scroll.content_height = total;
        let max_offset = node.scroll.max_offset(content_size.h);
        node.scroll.offset = node.scroll.offset.clamp(0.0, max_offset);
        layout_origin.y = content_box.y - node.scroll.offset;
    }

    // 4. Place children.
    for (index, child) in node.children.iter_mut().enumerate() {
        if !child.visible {
            child.bounds = Rect::ZERO;
            continue;
        }
        // Already placed by the absolute pass above; the flow must not move it.
        if child.style.position.is_some() {
            continue;
        }
        let size = sizes[index];
        let margin = child.style.margin;
        let cross_available = if horizontal { content_size.h } else { content_size.w };
        let cross_margin = if horizontal { margin.vertical() } else { margin.horizontal() };
        let cross_space = (cross_available - cross_margin).max(0.0);

        // Stretch fills the cross axis when the child did not ask for an explicit
        // size there; an explicit Width/Height always wins, matching WPF.
        let cross_is_auto = if horizontal {
            child.style.height == Length::Auto
        } else {
            child.style.width == Length::Auto
        };
        let stretch_cross = child.style.fills_cross(horizontal)
            || (node.style.align == Align::Stretch && cross_is_auto);

        let (child_w, child_h) = if stretch_cross {
            if horizontal {
                (size.w, cross_space)
            } else {
                (cross_space, size.h)
            }
        } else {
            (size.w, size.h)
        };

        let cross_extent = if horizontal { child_h + cross_margin } else { child_w + cross_margin };
        let cross_offset = if stretch_cross {
            0.0
        } else {
            node.style.align.offset(cross_available, cross_extent)
        };

        let (x, y) = if horizontal {
            (layout_origin.x + cursor + margin.left, layout_origin.y + cross_offset + margin.top)
        } else {
            (layout_origin.x + cross_offset + margin.left, layout_origin.y + cursor + margin.top)
        };

        arrange(child, Rect::new(x, y, child_w, child_h), metrics);
        cursor += main_of(Size::new(child_w, child_h), horizontal)
            + main_margin_of(margin, horizontal)
            + gap
            + gap_extra;
    }
}

fn main_of(size: Size, horizontal: bool) -> f32 {
    if horizontal {
        size.w
    } else {
        size.h
    }
}

fn main_margin_of(margin: Edges, horizontal: bool) -> f32 {
    if horizontal {
        margin.horizontal()
    } else {
        margin.vertical()
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

/// Resolves a length against an available extent.
pub fn resolve(length: Length, available: f32) -> Option<f32> {
    length.resolve(available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node;

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
        let mut root = node::column().child(node::panel().size(Length::Percent(0.5), Length::Px(10.0)));
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
    fn vertical_and_horizontal_are_distinct() {
        assert!(clover_core::Orientation::Horizontal.is_horizontal());
        assert!(!clover_core::Orientation::Vertical.is_horizontal());
    }

    // ---- fill ----------------------------------------------------------

    #[test]
    fn fill_child_absorbs_leftover_height() {
        let mut root = node::column()
            .child(node::panel().size(Length::Auto, Length::Px(30.0)))
            .child(node::panel().size(Length::Auto, Length::Fill));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.h, 30.0);
        assert_eq!(root.children[1].bounds.h, 70.0, "leftover height goes to the fill child");
        assert_eq!(root.children[1].bounds.y, 30.0);
    }

    #[test]
    fn fill_children_share_leftover_equally() {
        let mut root = node::row()
            .child(node::panel().size(Length::Px(20.0), Length::Auto))
            .child(node::panel().size(Length::Fill, Length::Auto))
            .child(node::panel().size(Length::Fill, Length::Auto));
        layout(&mut root, size(100.0, 40.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.w, 20.0);
        assert_eq!(root.children[1].bounds.w, 40.0);
        assert_eq!(root.children[2].bounds.w, 40.0);
        assert_eq!(root.children[2].bounds.x, 60.0);
    }

    #[test]
    fn fill_respects_gaps_and_margins() {
        let mut root = node::row()
            .gap(10.0)
            .child(node::panel().size(Length::Px(10.0), Length::Auto))
            .child(
                node::panel()
                    .size(Length::Fill, Length::Auto)
                    .margin(Edges::new(5.0, 0.0, 5.0, 0.0)),
            );
        layout(&mut root, size(100.0, 40.0), &EmptyMetrics);
        // 100 - 10 (fixed) - 10 (gap) = 80 leftover, minus 10 margin = 70.
        assert_eq!(root.children[1].bounds.w, 70.0);
        // 10 (fixed) + 10 (gap) puts the slot at 20; the left margin offsets the
        // child inside its slot, so it starts at 25 and ends at 95.
        assert_eq!(root.children[1].bounds.x, 25.0);
        assert_eq!(root.children[1].bounds.right(), 95.0);
    }

    #[test]
    fn fill_on_cross_axis_stretches() {
        let mut root = node::column().child(node::panel().size(Length::Fill, Length::Px(10.0)));
        layout(&mut root, size(80.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.w, 80.0);
    }

    #[test]
    fn explicit_size_beats_fill_sibling_distribution() {
        let mut root = node::row()
            .child(node::panel().size(Length::Px(60.0), Length::Auto))
            .child(node::panel().size(Length::Fill, Length::Auto))
            .child(node::panel().size(Length::Px(60.0), Length::Auto));
        layout(&mut root, size(100.0, 40.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.w, 60.0);
        assert_eq!(root.children[1].bounds.w, 0.0, "over-subscribed: fill gets nothing");
        assert_eq!(root.children[2].bounds.w, 60.0);
    }

    #[test]
    fn nested_fill_propagates_through_containers() {
        let mut root = node::column()
            .child(
                node::row()
                    .size(Length::Auto, Length::Fill)
                    .child(node::panel().size(Length::Fill, Length::Auto)),
            )
            .child(node::panel().size(Length::Auto, Length::Px(20.0)));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        let row = &root.children[0];
        assert_eq!(row.bounds.h, 80.0);
        assert_eq!(row.children[0].bounds.w, 100.0, "inner fill spans the row");
    }

    #[test]
    fn absolute_position_ignores_the_flow() {
        let mut root = node::column()
            .size(Length::Px(200.0), Length::Px(200.0))
            .child(node::panel().size(Length::Px(50.0), Length::Px(50.0)).name("flow"))
            .child(
                node::panel()
                    .size(Length::Px(30.0), Length::Px(30.0))
                    .position(120.0, 40.0)
                    .name("floating"),
            )
            .child(node::panel().size(Length::Px(50.0), Length::Px(50.0)).name("after"));
        layout(&mut root, size(200.0, 200.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds, Rect::new(0.0, 0.0, 50.0, 50.0));
        assert_eq!(
            root.children[2].bounds,
            Rect::new(0.0, 50.0, 50.0, 50.0),
            "the next flow child is not pushed down by the floating one"
        );
        assert_eq!(
            root.children[1].bounds,
            Rect::new(120.0, 40.0, 30.0, 30.0),
            "absolute position is relative to the content box"
        );
    }

    #[test]
    fn absolute_position_respects_padding_and_fill() {
        let mut root = node::column()
            .size(Length::Px(100.0), Length::Px(100.0))
            .padding(Edges::all(10.0))
            .child(
                node::panel()
                    .size(Length::Fill, Length::Px(20.0))
                    .position(5.0, 15.0),
            );
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds, Rect::new(15.0, 25.0, 80.0, 20.0));
    }

    #[test]
    fn grid_places_children_in_uniform_cells() {
        let mut root = node::grid(2)
            .size(Length::Px(200.0), Length::Px(100.0))
            .child(node::panel().name("a"))
            .child(node::panel().name("b"))
            .child(node::panel().name("c"));
        layout(&mut root, size(200.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds, Rect::new(0.0, 0.0, 100.0, 50.0));
        assert_eq!(root.children[1].bounds, Rect::new(100.0, 0.0, 100.0, 50.0));
        assert_eq!(root.children[2].bounds, Rect::new(0.0, 50.0, 100.0, 50.0));
    }

    #[test]
    fn grid_skips_hidden_children() {
        let mut root = node::grid(2)
            .size(Length::Px(200.0), Length::Px(100.0))
            .child(node::panel().name("a"))
            .child(node::panel().name("hidden").visible(false))
            .child(node::panel().name("c"));
        layout(&mut root, size(200.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[2].bounds.x, 100.0, "c takes the second cell");
    }

    #[test]
    fn grid_measures_rows_from_content() {
        let mut root = node::grid(2).child(
            node::panel()
                .size(Length::Auto, Length::Px(30.0))
                .margin(Edges::all(5.0)),
        );
        layout(&mut root, size(200.0, 400.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.h, 30.0);
        assert_eq!(root.children[0].bounds.x, 5.0);
    }

    #[test]
    fn scrollable_container_measures_content() {
        let mut root = node::column()
            .size(Length::Px(100.0), Length::Px(50.0))
            .scrollable()
            .child(node::panel().size(Length::Auto, Length::Px(40.0)))
            .child(node::panel().size(Length::Auto, Length::Px(40.0)))
            .child(node::panel().size(Length::Auto, Length::Px(40.0)));
        layout(&mut root, size(100.0, 50.0), &EmptyMetrics);
        assert_eq!(root.scroll.content_height, 120.0);
        assert_eq!(root.scroll.max_offset(50.0), 70.0);
        assert!(root.scroll.is_scrollable(50.0));
    }

    #[test]
    fn scroll_offset_shifts_children_up() {
        let mut root = node::column()
            .size(Length::Px(100.0), Length::Px(50.0))
            .scrollable()
            .child(node::panel().size(Length::Auto, Length::Px(40.0)))
            .child(node::panel().size(Length::Auto, Length::Px(40.0)));
        layout(&mut root, size(100.0, 50.0), &EmptyMetrics);
        assert_eq!(root.scroll.content_height, 80.0);
        assert_eq!(root.children[1].bounds.y, 40.0);

        root.scroll.offset = 20.0;
        layout(&mut root, size(100.0, 50.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.y, -20.0, "content shifts up by the offset");
        assert_eq!(root.children[1].bounds.y, 20.0);

        root.scroll.offset = 999.0;
        layout(&mut root, size(100.0, 50.0), &EmptyMetrics);
        assert_eq!(root.scroll.offset, 30.0, "offset clamps to content - viewport");
    }

    #[test]
    fn scroll_viewport_never_inflates_its_parent() {
        // An Auto-height parent holding a Fill-height scroll viewport used to
        // resolve Fill against the content height, making the viewport as tall as
        // its content and therefore unscrollable.
        let mut root = node::column().size(Length::Px(100.0), Length::Px(100.0)).child(
            node::column().child(
                node::column()
                    .height(Length::Fill)
                    .scrollable()
                    .child(node::panel().size(Length::Auto, Length::Px(40.0)))
                    .child(node::panel().size(Length::Auto, Length::Px(40.0)))
                    .child(node::panel().size(Length::Auto, Length::Px(40.0))),
            ),
        );
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        let viewport = &root.children[0].children[0];
        assert_eq!(viewport.bounds.h, 100.0, "viewport is capped by the offered height");
        assert_eq!(viewport.scroll.content_height, 120.0);
        assert!(viewport.scroll.is_scrollable(viewport.bounds.h), "content exceeds the viewport");
    }

    #[test]
    fn justify_still_works_without_fill() {
        let mut root = node::column()
            .justify(Align::End)
            .child(node::panel().size(Length::Px(10.0), Length::Px(10.0)));
        layout(&mut root, size(100.0, 100.0), &EmptyMetrics);
        assert_eq!(root.children[0].bounds.y, 90.0);
    }
}
