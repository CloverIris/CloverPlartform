//! Display list construction.
//!
//! The tree is flattened once per frame into a list of commands in **physical
//! pixels**. Everything the rasteriser needs (shaped glyphs, decoded images) is
//! resolved here, so tiles can be rendered later from any thread with no shared
//! mutable state.

use std::sync::Arc;

use clover_core::{Align, Color, Point, Premul, Rect, Size};
use clover_ui::{ImageHandle, Node, SurfaceHandle, UiTree, View};

use crate::canvas::ImagePixels;
use crate::image::ImageStore;
use crate::text::{TextEngine, TextRun};

/// A resolved drawing command, all coordinates in physical pixels.
#[derive(Clone)]
pub enum DrawCmd {
    RoundedRect {
        rect: Rect,
        radius: f32,
        color: Premul,
        clip: Rect,
    },
    Border {
        rect: Rect,
        radius: f32,
        width: f32,
        color: Premul,
        clip: Rect,
    },
    Image {
        rect: Rect,
        image: Arc<ImagePixels>,
        clip: Rect,
        /// Multiplied into every texel when blitting.
        opacity: f32,
    },
    Text {
        run: Arc<TextRun>,
        color: Premul,
        clip: Rect,
    },
    /// A vector path, rasterised at draw time.
    Path {
        geometry: Arc<clover_core::PathGeometry>,
        /// Physical position of the path's own origin.
        origin: Point,
        /// Window scale, so the path is crisp at 2x.
        scale: f32,
        fill: Option<Premul>,
        stroke: Option<Premul>,
        stroke_width: f32,
        clip: Rect,
    },
    /// A gradient-filled box.
    Gradient {
        rect: Rect,
        radius: f32,
        geometry: clover_ui::style::GradientGeometry,
        stops: Arc<Vec<clover_ui::style::GradientStop>>,
        clip: Rect,
    },
    /// A child surface the compositor samples; the tile renderer skips it.
    Surface {
        rect: Rect,
        handle: SurfaceHandle,
        clip: Rect,
    },
}

/// A frame's worth of resolved commands.
pub struct DisplayList {
    pub cmds: Vec<DrawCmd>,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

impl DisplayList {
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Every surface command, for the compositor to fill in.
    pub fn surfaces(&self) -> impl Iterator<Item = (SurfaceHandle, Rect, Rect)> + '_ {
        self.cmds.iter().filter_map(|cmd| match cmd {
            DrawCmd::Surface { rect, handle, clip } => Some((*handle, *rect, *clip)),
            _ => None,
        })
    }
}

/// Flattens `tree` into a display list at `scale`.
pub fn build(tree: &UiTree, scale: f32, text: &TextEngine, images: &ImageStore) -> DisplayList {
    let scale = scale.max(0.01);
    let width = (tree.viewport().w * scale).round().max(1.0) as u32;
    let height = (tree.viewport().h * scale).round().max(1.0) as u32;
    let canvas = Rect::new(0.0, 0.0, width as f32, height as f32);

    let mut cmds = Vec::new();
    walk(tree.root(), scale, canvas, &mut cmds, text, images);
    // The overlay paints last so popups are never clipped by their host.
    if let Some(overlay) = tree.overlay() {
        walk(overlay, scale, canvas, &mut cmds, text, images);
    }
    DisplayList { cmds, width, height, scale }
}

fn walk(
    node: &Node,
    scale: f32,
    clip: Rect,
    out: &mut Vec<DrawCmd>,
    text: &TextEngine,
    images: &ImageStore,
) {
    if !node.visible {
        return;
    }
    let rect = node.bounds.scale(scale);
    if clip.is_empty() || rect.is_empty() {
        return;
    }

    let opacity = node.style.opacity.clamp(0.0, 1.0);
    let with_opacity = |c: Color| -> Premul {
        if opacity >= 1.0 {
            c.premultiply()
        } else {
            c.scale_alpha(opacity).premultiply()
        }
    };

    match &node.style.brush {
        Some(clover_ui::style::Brush::Texture { handle, fit, opacity }) => {
            if let Some(image) = images.get(*handle) {
                let natural = Size::new(image.width as f32, image.height as f32);
                let fitted = fit_rect(rect, *fit, natural);
                // A background texture never paints outside its own control:
                // `Cover` crops, it does not overflow into the neighbours.
                let brush_clip = clip.intersect(rect);
                if !brush_clip.is_empty() {
                    out.push(DrawCmd::Image {
                        rect: fitted,
                        image: Arc::clone(image),
                        clip: brush_clip,
                        opacity: *opacity,
                    });
                }
            }
        }
        Some(clover_ui::style::Brush::Gradient { geometry, stops }) if stops.len() >= 2 => {
            let brush_clip = clip.intersect(rect);
            if !brush_clip.is_empty() {
                out.push(DrawCmd::Gradient {
                    rect,
                    radius: node.style.radius * scale,
                    geometry: *geometry,
                    stops: Arc::new(stops.clone()),
                    clip: brush_clip,
                });
            }
        }
        Some(clover_ui::style::Brush::Solid(color)) => {
            if color.a > 0 {
                out.push(DrawCmd::RoundedRect {
                    rect,
                    radius: node.style.radius * scale,
                    color: with_opacity(*color),
                    clip,
                });
            }
        }
        _ => {
            if node.style.background.a > 0 {
                // Default interaction feedback: hovered surfaces lift, pressed
                // ones sink. Skins that want their own visuals set the background
                // to transparent and draw their own state on top.
                let background = if node.state.pressed {
                    node.style.background.lerp(Color::BLACK, 0.18)
                } else if node.state.hovered {
                    node.style.background.lerp(Color::WHITE, 0.08)
                } else {
                    node.style.background
                };
                out.push(DrawCmd::RoundedRect {
                    rect,
                    radius: node.style.radius * scale,
                    color: with_opacity(background),
                    clip,
                });
            }
        }
    }

    if node.style.border_width > 0.0 && node.style.border_color.a > 0 {
        out.push(DrawCmd::Border {
            rect,
            radius: node.style.radius * scale,
            width: node.style.border_width * scale,
            color: with_opacity(node.style.border_color),
            clip,
        });
    }

    match &node.view {
        View::Text(content) => {
            if !content.is_empty() {
                let content_box = node.style.padding.deflate(node.bounds).scale(scale);
                let font_px = (node.style.font_size * scale).max(1.0);
                let line_height = text.line_height(font_px);
                let max_width = if node.style.wrap && content_box.w > 0.0 {
                    Some(content_box.w)
                } else {
                    None
                };
                let lines = text.wrap_lines(content, font_px, max_width);
                let block_height = line_height * lines.len() as f32;
                let top = content_box.y + ((content_box.h - block_height) * 0.5).max(0.0);
                let ascent = text.ascent(font_px);
                for (index, line) in lines.iter().enumerate() {
                    if line.is_empty() {
                        continue;
                    }
                    let line_width = text.line_width(line, font_px);
                    let x = content_box.x + node.style.text_align.offset(content_box.w, line_width);
                    let baseline = top + line_height * index as f32 + ascent;
                    let run = text.shape(line, font_px, x, baseline);
                    if !run.is_empty() {
                        out.push(DrawCmd::Text {
                            run: Arc::new(run),
                            color: with_opacity(node.style.text_color),
                            clip,
                        });
                    }
                }
            }
        }
        View::Image { handle, fit } => {
            if let Some(image) = images.get(*handle) {
                let content_box = node.style.padding.deflate(node.bounds).scale(scale);
                let fitted = fit_rect(content_box, *fit, Size::new(image.width as f32, image.height as f32));
                out.push(DrawCmd::Image { rect: fitted, image: Arc::clone(image), clip, opacity: 1.0 });
            }
        }
        View::TextInput { text: value, placeholder, caret, composition } => {
            let content_box = node.style.padding.deflate(node.bounds).scale(scale);
            let font_px = (node.style.font_size * scale).max(1.0);
            let empty = value.is_empty() && composition.is_none();
            let (displayed, composition_range) =
                clover_ui::node::text_input_display(value, composition.as_ref(), *caret);
            let shown: &str = if empty { placeholder } else { displayed.as_str() };
            let color = if empty {
                node.style.text_color.scale_alpha(0.55)
            } else {
                node.style.text_color
            };
            let line_height = text.line_height(font_px);
            let baseline = content_box.y + ((content_box.h - line_height) * 0.5).max(0.0)
                + text.ascent(font_px);
            if !shown.is_empty() {
                let run = text.shape(shown, font_px, content_box.x, baseline);
                if !run.is_empty() {
                    out.push(DrawCmd::Text { run: Arc::new(run), color: with_opacity(color), clip });
                }
            }

            // Composition preview gets an underline, the conventional IME cue.
            if let Some((start, end)) = composition_range {
                let left = content_box.x + text.line_width(&shown[..start.min(shown.len())], font_px);
                let right = content_box.x + text.line_width(&shown[..end.min(shown.len())], font_px);
                let underline = Rect::new(
                    left,
                    baseline + 2.0 * scale,
                    (right - left).max(1.0),
                    (1.5 * scale).max(1.0),
                );
                out.push(DrawCmd::RoundedRect {
                    rect: underline,
                    radius: 0.0,
                    color: with_opacity(node.style.text_color),
                    clip,
                });
            }

            // Caret, only while focused: 1px bar at the caret offset, which lands
            // after the composition while one is active.
            if node.state.focused {
                let caret_byte = composition_range.map(|(_, end)| end).unwrap_or_else(|| {
                    (*caret).min(value.len())
                });
                let caret_x = content_box.x + text.line_width(&shown[..caret_byte.min(shown.len())], font_px);
                let caret_top = content_box.y + 2.0 * scale;
                let caret_height = (content_box.h - 4.0 * scale).max(font_px);
                out.push(DrawCmd::RoundedRect {
                    rect: Rect::new(caret_x, caret_top, (1.5 * scale).max(1.0), caret_height),
                    radius: 0.0,
                    color: with_opacity(node.style.text_color),
                    clip,
                });
            }
        }
        View::Path { geometry, fill, stroke, stroke_width } => {
            if fill.is_some() || *stroke_width > 0.0 {
                let content_box = node.style.padding.deflate(node.bounds);
                out.push(DrawCmd::Path {
                    geometry: Arc::clone(geometry),
                    origin: Point::new(content_box.x * scale, content_box.y * scale),
                    scale,
                    fill: fill.as_ref().map(|color| with_opacity(*color)),
                    stroke: stroke.as_ref().map(|color| with_opacity(*color)),
                    stroke_width: *stroke_width * scale,
                    clip,
                });
            }
        }
        View::Surface { handle } => {
            let content_box = node.style.padding.deflate(node.bounds).scale(scale);
            out.push(DrawCmd::Surface { rect: content_box, handle: *handle, clip });
        }
        View::Empty => {}
    }

    let child_clip = if node.style.clip {
        let clipped = clip.intersect(rect);
        if clipped.is_empty() {
            return;
        }
        clipped
    } else {
        clip
    };

    for child in &node.children {
        walk(child, scale, child_clip, out, text, images);
    }

    // The scrollbar paints on top of the content it belongs to.
    if node.scroll.enabled {
        push_scrollbar(node, rect, scale, child_clip, out);
    }
}

fn push_scrollbar(node: &Node, rect: Rect, scale: f32, clip: Rect, out: &mut Vec<DrawCmd>) {
    let viewport = node.bounds.h;
    let content = node.scroll.content_height;
    if content <= viewport + 0.5 || viewport <= 0.0 {
        return;
    }
    let bar_width = (6.0 * scale).max(3.0);
    let inset = 2.0 * scale;
    let track = Rect::new(
        rect.right() - bar_width - inset,
        rect.y + inset,
        bar_width,
        (rect.h - inset * 2.0).max(1.0),
    );
    if track.is_empty() {
        return;
    }
    out.push(DrawCmd::RoundedRect {
        rect: track,
        radius: bar_width * 0.5,
        color: Color::rgba(255, 255, 255, 18).premultiply(),
        clip,
    });

    let visible_ratio = (viewport / content).clamp(0.0, 1.0);
    let thumb_height = (track.h * visible_ratio).max(bar_width * 2.0).min(track.h);
    let max_offset = (content - viewport).max(0.001);
    let progress = (node.scroll.offset / max_offset).clamp(0.0, 1.0);
    let thumb = Rect::new(
        track.x,
        track.y + (track.h - thumb_height) * progress,
        track.w,
        thumb_height,
    );
    out.push(DrawCmd::RoundedRect {
        rect: thumb,
        radius: bar_width * 0.5,
        color: Color::rgba(255, 255, 255, 72).premultiply(),
        clip,
    });
}

/// Fits an image of `natural` size into `box_rect` according to `fit`.
fn fit_rect(box_rect: Rect, fit: clover_ui::Fit, natural: Size) -> Rect {
    use clover_ui::Fit;
    if natural.is_empty() || box_rect.is_empty() {
        return box_rect;
    }
    match fit {
        Fit::Fill => box_rect,
        Fit::Contain => {
            let scale = (box_rect.w / natural.w).min(box_rect.h / natural.h);
            let w = natural.w * scale;
            let h = natural.h * scale;
            Rect::new(
                box_rect.x + (box_rect.w - w) * 0.5,
                box_rect.y + (box_rect.h - h) * 0.5,
                w,
                h,
            )
        }
        Fit::Cover => {
            let scale = (box_rect.w / natural.w).max(box_rect.h / natural.h);
            let w = natural.w * scale;
            let h = natural.h * scale;
            Rect::new(
                box_rect.x + (box_rect.w - w) * 0.5,
                box_rect.y + (box_rect.h - h) * 0.5,
                w,
                h,
            )
        }
    }
}

/// Convenience: the image handle used by a node, if any.
pub fn node_image(node: &Node) -> Option<ImageHandle> {
    match &node.view {
        View::Image { handle, .. } => Some(*handle),
        _ => None,
    }
}

/// Convenience: text alignment helper re-exported for skins.
pub fn align_offset(align: Align, available: f32, extent: f32) -> f32 {
    align.offset(available, extent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clover_core::Edges;
    use clover_ui::{layout::EmptyMetrics, node, style::Length, Node};

    fn engine() -> TextEngine {
        TextEngine::load_system_default().expect("a system font is required for these tests")
    }

    fn build_tree(root: Node) -> UiTree {
        let mut tree = UiTree::new(root);
        tree.resize(Size::new(100.0, 100.0), 2.0);
        tree.layout(&EmptyMetrics);
        tree
    }

    #[test]
    fn background_becomes_a_command() {
        let tree = build_tree(node::panel().background(Color::rgb(10, 10, 10)));
        let list = build(&tree, 2.0, &engine(), &ImageStore::new());
        assert_eq!(list.cmds.len(), 1);
        match &list.cmds[0] {
            DrawCmd::RoundedRect { rect, .. } => {
                assert_eq!((rect.w, rect.h), (200.0, 200.0), "logical 100x100 at scale 2");
            }
            _ => panic!("expected a rect"),
        }
    }

    #[test]
    fn nested_clip_intersects_children() {
        // A clipped 40x40 box at (10,10) holding an oversized white child: the
        // child is clipped to the inner box, not to the outer border box.
        let tree = build_tree(
            node::panel().clip(true).child(
                node::panel()
                    .size(Length::Px(40.0), Length::Px(40.0))
                    .margin(Edges::all(10.0))
                    .clip(true)
                    .child(
                        node::panel()
                            .size(Length::Px(200.0), Length::Px(200.0))
                            .background(Color::WHITE),
                    ),
            ),
        );
        let list = build(&tree, 1.0, &engine(), &ImageStore::new());
        let white = list
            .cmds
            .iter()
            .find_map(|cmd| match cmd {
                DrawCmd::RoundedRect { color, clip, .. } if *color == Color::WHITE.premultiply() => Some(*clip),
                _ => None,
            })
            .expect("white rect command");
        assert_eq!(white, Rect::new(10.0, 10.0, 40.0, 40.0));
    }

    #[test]
    fn text_command_carries_shaped_glyphs() {
        let tree = build_tree(node::text("Hi").font_size(20.0));
        let list = build(&tree, 1.0, &engine(), &ImageStore::new());
        let has_text = list.cmds.iter().any(|c| matches!(c, DrawCmd::Text { .. }));
        assert!(has_text, "text node must produce a text command");
        if let Some(DrawCmd::Text { run, .. }) = list.cmds.iter().find(|c| matches!(c, DrawCmd::Text { .. })) {
            assert_eq!(run.glyphs.len(), 2);
        }
    }

    #[test]
    fn surface_command_is_reported() {
        let tree = build_tree(node::direct_surface(SurfaceHandle(9)));
        let list = build(&tree, 1.0, &engine(), &ImageStore::new());
        let surfaces: Vec<_> = list.surfaces().collect();
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].0, SurfaceHandle(9));
    }

    #[test]
    fn hidden_nodes_produce_nothing() {
        let tree = build_tree(node::panel().background(Color::WHITE).visible(false));
        let list = build(&tree, 1.0, &engine(), &ImageStore::new());
        assert!(list.is_empty());
    }

    #[test]
    fn fit_contain_preserves_aspect() {
        let fitted = fit_rect(Rect::new(0.0, 0.0, 100.0, 100.0), clover_ui::Fit::Contain, Size::new(50.0, 25.0));
        assert_eq!(fitted.w, 100.0);
        assert_eq!(fitted.h, 50.0);
        assert_eq!(fitted.y, 25.0);
    }
}
