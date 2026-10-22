//! Tile-parallel CPU rasteriser.
//!
//! Damage rectangles are split into tiles and rendered concurrently with rayon.
//! A tile renders into its own small canvas, which is then copied into the frame
//! buffer. That keeps the parallel section free of `unsafe` and of shared mutable
//! state, at the cost of one extra copy per damaged pixel.

use rayon::prelude::*;

use clover_core::{Premul, Rect, Size};
use clover_ui::{ImageHandle, LayoutMetrics, UiTree};

use crate::canvas::Canvas;
use crate::display_list::{build, DisplayList, DrawCmd};
use crate::image::ImageStore;
use crate::text::TextEngine;

/// Largest tile edge in physical pixels.
const TILE_EDGE: f32 = 256.0;

/// Owns the frame buffer, the text engine and the image registry.
pub struct CpuRenderer {
    pub text: TextEngine,
    pub images: ImageStore,
    canvas: Canvas,
    last_tile_count: usize,
}

impl CpuRenderer {
    /// Creates a renderer backed by a system font.
    ///
    /// Returns `None` when no usable font is installed, which callers should
    /// surface as a startup error rather than silently rendering no text.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Self {
            text: TextEngine::load_system_default()?,
            images: ImageStore::new(),
            canvas: Canvas::new(width, height),
            last_tile_count: 0,
        })
    }

    /// Creates a renderer around an explicit text engine, for tests and for hosts
    /// that ship their own font.
    pub fn with_text_engine(text: TextEngine, width: u32, height: u32) -> Self {
        Self { text, images: ImageStore::new(), canvas: Canvas::new(width, height), last_tile_count: 0 }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.canvas.resize(width, height);
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    pub fn canvas_mut(&mut self) -> &mut Canvas {
        &mut self.canvas
    }

    /// Number of tiles rendered by the last frame, for diagnostics.
    pub fn last_tile_count(&self) -> usize {
        self.last_tile_count
    }

    /// Builds a display list for `tree` and renders the damaged regions.
    pub fn render_tree(&mut self, tree: &UiTree, damage: &[Rect], clear: Premul) {
        let list = build(tree, tree.scale(), &self.text, &self.images);
        self.render(&list, damage, clear);
    }

    /// Rasterises `list` into the damaged regions.
    ///
    /// `damage` is in physical pixels; an empty slice renders nothing.
    pub fn render(&mut self, list: &DisplayList, damage: &[Rect], clear: Premul) {
        if self.canvas.width != list.width || self.canvas.height != list.height {
            self.resize(list.width, list.height);
        }
        let bounds = Rect::new(0.0, 0.0, self.canvas.width as f32, self.canvas.height as f32);

        let mut tiles: Vec<Rect> = Vec::new();
        for rect in damage {
            let clipped = rect.snap_out().intersect(bounds);
            if !clipped.is_empty() {
                tiles.extend(split_tiles(clipped, TILE_EDGE));
            }
        }
        self.last_tile_count = tiles.len();
        if tiles.is_empty() {
            return;
        }

        let rendered: Vec<(Rect, Canvas)> = tiles
            .par_iter()
            .map(|tile| (*tile, render_tile(list, *tile, clear)))
            .collect();

        let width = self.canvas.width as usize;
        let height = self.canvas.height as usize;
        let dest = self.canvas.data_mut();
        for (rect, tile) in rendered {
            let tile_w = tile.width as usize;
            let tile_h = tile.height as usize;
            let source = tile.data();
            let x = rect.x as usize;
            let y = rect.y as usize;
            for row in 0..tile_h {
                let dst_row = y + row;
                if dst_row >= height {
                    break;
                }
                let src_start = row * tile_w;
                let dst_start = dst_row * width + x;
                let count = tile_w.min(width.saturating_sub(x));
                dest[dst_start..dst_start + count]
                    .copy_from_slice(&source[src_start..src_start + count]);
            }
        }
    }
}

impl LayoutMetrics for CpuRenderer {
    fn measure_text(&self, text: &str, font_px: f32, max_width: Option<f32>) -> Size {
        self.text.measure(text, font_px, max_width)
    }

    fn line_height(&self, font_px: f32) -> f32 {
        self.text.line_height(font_px)
    }

    fn measure_image(&self, handle: ImageHandle) -> Size {
        self.images.measure(handle)
    }
}

/// Splits `rect` into tiles no larger than `max_edge` pixels on a side.
pub fn split_tiles(rect: Rect, max_edge: f32) -> Vec<Rect> {
    let mut tiles = Vec::new();
    let mut y = rect.y;
    while y < rect.bottom() {
        let height = (rect.bottom() - y).min(max_edge);
        let mut x = rect.x;
        while x < rect.right() {
            let width = (rect.right() - x).min(max_edge);
            tiles.push(Rect::new(x, y, width, height));
            x += width;
        }
        y += height;
    }
    tiles
}

fn render_tile(list: &DisplayList, tile: Rect, clear: Premul) -> Canvas {
    let mut canvas = Canvas::new(tile.w.max(1.0) as u32, tile.h.max(1.0) as u32);
    canvas.clear(clear);
    let local_bounds = canvas.bounds();
    let dx = -tile.x;
    let dy = -tile.y;

    for cmd in &list.cmds {
        match cmd {
            DrawCmd::RoundedRect { rect, radius, color, clip } => {
                let r = rect.offset(dx, dy);
                let c = clip.offset(dx, dy).intersect(local_bounds);
                if c.is_empty() {
                    continue;
                }
                canvas.fill_round_rect(r, *radius, *color, c);
            }
            DrawCmd::Border { rect, radius, width, color, clip } => {
                let r = rect.offset(dx, dy);
                let c = clip.offset(dx, dy).intersect(local_bounds);
                if c.is_empty() {
                    continue;
                }
                canvas.stroke_rect(r, *radius, *width, *color, c);
            }
            DrawCmd::Image { rect, image, clip } => {
                let r = rect.offset(dx, dy);
                let c = clip.offset(dx, dy).intersect(local_bounds);
                if c.is_empty() {
                    continue;
                }
                canvas.draw_image(r, image, c);
            }
            DrawCmd::Text { run, color, clip } => {
                let c = clip.offset(dx, dy).intersect(local_bounds);
                if c.is_empty() {
                    continue;
                }
                for glyph in &run.glyphs {
                    draw_glyph(&mut canvas, glyph.x + dx, glyph.y + dy, &glyph.bitmap, *color, c);
                }
            }
            DrawCmd::Surface { .. } => {
                // Filled in by the compositor, not by a tile.
            }
        }
    }
    canvas
}

fn draw_glyph(
    canvas: &mut Canvas,
    x: f32,
    y: f32,
    bitmap: &crate::text::GlyphBitmap,
    color: Premul,
    clip: Rect,
) {
    if bitmap.width == 0 || bitmap.height == 0 {
        return;
    }
    let origin_x = x.floor() as i32;
    let origin_y = y.floor() as i32;
    for row in 0..bitmap.height as i32 {
        for col in 0..bitmap.width as i32 {
            let coverage = bitmap.coverage[(row as usize) * (bitmap.width as usize) + col as usize];
            if coverage == 0 {
                continue;
            }
            canvas.blend(origin_x + col, origin_y + row, color.with_coverage(coverage), clip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clover_core::Color;
    use clover_ui::{node, style::Length, UiTree};

    fn renderer() -> CpuRenderer {
        let text = TextEngine::load_system_default().expect("system font required");
        CpuRenderer::with_text_engine(text, 64, 64)
    }

    fn tree_with(root: clover_ui::Node, w: f32, h: f32) -> UiTree {
        let mut tree = UiTree::new(root);
        tree.resize(Size::new(w, h), 1.0);
        tree
    }

    #[test]
    fn split_tiles_covers_exact_area() {
        let tiles = split_tiles(Rect::new(0.0, 0.0, 512.0, 256.0), 256.0);
        assert_eq!(tiles.len(), 2);
        let total: f32 = tiles.iter().map(|t| t.w * t.h).sum();
        assert_eq!(total, 512.0 * 256.0);
    }

    #[test]
    fn renders_background_into_damage() {
        let mut renderer = renderer();
        let mut tree = tree_with(
            node::panel().background(Color::rgb(200, 40, 40)),
            64.0,
            64.0,
        );
        tree.layout(&renderer);
        renderer.render_tree(&tree, &[Rect::new(0.0, 0.0, 64.0, 64.0)], Premul::TRANSPARENT);
        let pixel = renderer.canvas().pixel(32, 32).to_color();
        assert_eq!(pixel, Color::rgb(200, 40, 40));
    }

    #[test]
    fn only_damaged_regions_are_written() {
        let mut renderer = renderer();
        let tree = tree_with(node::panel().background(Color::rgb(10, 200, 10)), 64.0, 64.0);
        let mut tree = tree;
        tree.layout(&renderer);
        renderer.render_tree(&tree, &[Rect::new(0.0, 0.0, 16.0, 16.0)], Premul::TRANSPARENT);
        assert_eq!(renderer.canvas().pixel(8, 8).a(), 255, "damaged tile rendered");
        assert_eq!(renderer.canvas().pixel(48, 48).a(), 0, "untouched tile stays clear");
    }

    #[test]
    fn tile_count_is_reported() {
        let mut renderer = renderer();
        let mut tree = tree_with(node::panel().background(Color::WHITE), 600.0, 600.0);
        tree.layout(&renderer);
        renderer.render_tree(&tree, &[Rect::new(0.0, 0.0, 600.0, 600.0)], Premul::TRANSPARENT);
        // 600 / 256 -> 3 tiles per axis.
        assert_eq!(renderer.last_tile_count(), 9);
    }

    #[test]
    fn overlapping_children_paint_in_order() {
        let mut renderer = renderer();
        let mut tree = tree_with(
            node::panel()
                .background(Color::BLACK)
                .child(
                    node::panel()
                        .size(Length::Px(32.0), Length::Px(32.0))
                        .background(Color::rgb(255, 0, 0)),
                ),
            64.0,
            64.0,
        );
        tree.layout(&renderer);
        renderer.render_tree(&tree, &[Rect::new(0.0, 0.0, 64.0, 64.0)], Premul::TRANSPARENT);
        assert_eq!(renderer.canvas().pixel(10, 10).to_color(), Color::rgb(255, 0, 0));
        assert_eq!(renderer.canvas().pixel(50, 50).to_color(), Color::BLACK);
    }

    #[test]
    fn text_renders_visible_pixels() {
        let mut renderer = renderer();
        let mut tree = tree_with(
            node::text("L").font_size(40.0).text_color(Color::WHITE),
            64.0,
            64.0,
        );
        tree.layout(&renderer);
        renderer.render_tree(&tree, &[Rect::new(0.0, 0.0, 64.0, 64.0)], Premul::TRANSPARENT);
        let any_ink = (0..64).any(|y| (0..64).any(|x| renderer.canvas().pixel(x, y).a() > 0));
        assert!(any_ink, "text must draw something");
    }
}
