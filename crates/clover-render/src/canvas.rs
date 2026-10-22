//! CPU draw target.
//!
//! The canvas stores **premultiplied BGRA8 packed in `u32`**, which is the same
//! memory layout CloverPic's `RgbaFrame` uses, so a surface can be handed to the
//! native host without a conversion pass.

use clover_core::{Color, Premul, Rect};

/// A premultiplied BGRA8 raster target.
#[derive(Clone)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pixels: Vec<u32>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        let mut canvas = Self { width: 0, height: 0, pixels: Vec::new() };
        canvas.resize(width, height);
        canvas
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height && !self.pixels.is_empty() {
            return;
        }
        self.width = width;
        self.height = height;
        self.pixels = vec![0; (width as usize) * (height as usize)];
    }

    pub fn stride(&self) -> usize {
        self.width as usize
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    /// Raw premultiplied BGRA8 words, for handing to a native host.
    pub fn data(&self) -> &[u32] {
        &self.pixels
    }

    pub fn data_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    pub fn clear(&mut self, color: Premul) {
        self.pixels.fill(color.0);
    }

    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> Premul {
        if x >= self.width || y >= self.height {
            return Premul::TRANSPARENT;
        }
        Premul(self.pixels[(y as usize) * (self.width as usize) + x as usize])
    }

    /// Blends one pixel, clipped to `clip`.
    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, color: Premul, clip: Rect) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let px = x as f32;
        let py = y as f32;
        if px < clip.x || py < clip.y || px >= clip.right() || py >= clip.bottom() {
            return;
        }
        let index = (y as usize) * (self.width as usize) + x as usize;
        let dst = Premul(self.pixels[index]);
        self.pixels[index] = color.over(dst).0;
    }

    /// Axis-aligned rectangle fill with exact area coverage on the edges.
    pub fn fill_rect(&mut self, rect: Rect, color: Premul, clip: Rect) {
        let area = rect.intersect(clip);
        if area.is_empty() || color.a() == 0 {
            return;
        }
        let x0 = area.x.floor() as i32;
        let y0 = area.y.floor() as i32;
        let x1 = area.right().ceil() as i32;
        let y1 = area.bottom().ceil() as i32;

        for y in y0..y1 {
            let cy = overlap(y as f32, y as f32 + 1.0, area.y, area.bottom());
            if cy <= 0.0 {
                continue;
            }
            for x in x0..x1 {
                let cx = overlap(x as f32, x as f32 + 1.0, area.x, area.right());
                if cx <= 0.0 {
                    continue;
                }
                let coverage = (cx * cy * 255.0).round().clamp(0.0, 255.0) as u8;
                self.blend(x, y, color.with_coverage(coverage), clip);
            }
        }
    }

    /// Rounded rectangle fill, antialiased with a signed distance field.
    ///
    /// `radius` is in physical pixels and is clamped to half the shorter side.
    pub fn fill_round_rect(&mut self, rect: Rect, radius: f32, color: Premul, clip: Rect) {
        if radius <= 0.5 {
            self.fill_rect(rect, color, clip);
            return;
        }
        let area = rect.intersect(clip);
        if area.is_empty() || color.a() == 0 {
            return;
        }
        let radius = radius.min(rect.w * 0.5).min(rect.h * 0.5);
        let x0 = area.x.floor() as i32;
        let y0 = area.y.floor() as i32;
        let x1 = area.right().ceil() as i32;
        let y1 = area.bottom().ceil() as i32;

        for y in y0..y1 {
            for x in x0..x1 {
                let cx = x as f32 + 0.5;
                let cy = y as f32 + 0.5;
                let distance = round_rect_sdf(cx, cy, rect, radius);
                // One pixel of antialiasing centred on the edge.
                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                self.blend(x, y, color.with_coverage((coverage * 255.0) as u8), clip);
            }
        }
    }

    /// Rectangle outline of `width` physical pixels, drawn inside `rect`.
    pub fn stroke_rect(&mut self, rect: Rect, radius: f32, width: f32, color: Premul, clip: Rect) {
        if width <= 0.0 || color.a() == 0 {
            return;
        }
        let width = width.min(rect.w * 0.5).min(rect.h * 0.5);
        let inner = Rect::new(
            rect.x + width,
            rect.y + width,
            (rect.w - width * 2.0).max(0.0),
            (rect.h - width * 2.0).max(0.0),
        );
        let radius = radius.min(rect.w * 0.5).min(rect.h * 0.5);
        let inner_radius = (radius - width).max(0.0);

        let area = rect.intersect(clip);
        if area.is_empty() {
            return;
        }
        let x0 = area.x.floor() as i32;
        let y0 = area.y.floor() as i32;
        let x1 = area.right().ceil() as i32;
        let y1 = area.bottom().ceil() as i32;

        for y in y0..y1 {
            for x in x0..x1 {
                let cx = x as f32 + 0.5;
                let cy = y as f32 + 0.5;
                let outer_d = round_rect_sdf(cx, cy, rect, radius);
                let inner_d = round_rect_sdf(cx, cy, inner, inner_radius);
                // Inside the outer shape but outside the inner one.
                let coverage = (0.5 - outer_d).clamp(0.0, 1.0) * (0.5 + inner_d).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                self.blend(x, y, color.with_coverage((coverage * 255.0) as u8), clip);
            }
        }
    }

    /// Blits a premultiplied source image with alpha, scaled into `dest`.
    ///
    /// Uses bilinear sampling when scaling so UI images stay smooth.
    pub fn draw_image(
        &mut self,
        dest: Rect,
        source: &ImagePixels,
        clip: Rect,
    ) {
        if source.width == 0 || source.height == 0 || source.pixels.is_empty() {
            return;
        }
        let area = dest.intersect(clip);
        if area.is_empty() {
            return;
        }
        let x0 = area.x.floor() as i32;
        let y0 = area.y.floor() as i32;
        let x1 = area.right().ceil() as i32;
        let y1 = area.bottom().ceil() as i32;

        let sx = source.width as f32 / dest.w.max(1.0);
        let sy = source.height as f32 / dest.h.max(1.0);

        for y in y0..y1 {
            for x in x0..x1 {
                // Sample at the destination pixel centre.
                let u = ((x as f32 + 0.5 - dest.x) * sx).clamp(0.0, source.width as f32 - 0.001);
                let v = ((y as f32 + 0.5 - dest.y) * sy).clamp(0.0, source.height as f32 - 0.001);
                let color = source.sample_bilinear(u, v);
                if color.a() == 0 {
                    continue;
                }
                self.blend(x, y, color, clip);
            }
        }
    }

    /// Blits a premultiplied buffer 1:1 at `x`,`y`.
    pub fn blit_premultiplied(&mut self, x: i32, y: i32, width: u32, height: u32, pixels: &[u32], clip: Rect) {
        for row in 0..height {
            for col in 0..width {
                let value = pixels[(row as usize) * (width as usize) + col as usize];
                if value == 0 {
                    continue;
                }
                self.blend(x + col as i32, y + row as i32, Premul(value), clip);
            }
        }
    }

    /// Converts to straight RGBA8 for PNG export.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for word in &self.pixels {
            let c = Premul(*word).to_color();
            out.extend_from_slice(&[c.r, c.g, c.b, c.a]);
        }
        out
    }

    /// Fills from straight RGBA8 (used by tests and the compositor).
    pub fn fill_from_rgba8(&mut self, rgba: &[u8]) {
        let count = (self.width as usize) * (self.height as usize);
        for i in 0..count.min(rgba.len() / 4) {
            let c = Color::rgba(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], rgba[i * 4 + 3]);
            self.pixels[i] = c.premultiply().0;
        }
    }
}

/// Overlap of `[a0, a1)` with `[b0, b1)`, in `0.0..=1.0`.
#[inline]
fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    let lo = a0.max(b0);
    let hi = a1.min(b1);
    (hi - lo).max(0.0)
}

/// Signed distance to a rounded rectangle: negative inside.
#[inline]
fn round_rect_sdf(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
    let half_w = rect.w * 0.5;
    let half_h = rect.h * 0.5;
    let cx = rect.x + half_w;
    let cy = rect.y + half_h;

    let qx = (px - cx).abs() - (half_w - radius);
    let qy = (py - cy).abs() - (half_h - radius);
    let outside_x = qx.max(0.0);
    let outside_y = qy.max(0.0);
    let outside = (outside_x * outside_x + outside_y * outside_y).sqrt();
    let inside = qx.max(qy).min(0.0);
    outside + inside - radius
}

/// A premultiplied image ready for blitting.
#[derive(Clone)]
pub struct ImagePixels {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

impl ImagePixels {
    /// Bilinear sample in pixel coordinates.
    pub fn sample_bilinear(&self, x: f32, y: f32) -> Premul {
        let x = x.clamp(0.0, self.width as f32 - 1.0);
        let y = y.clamp(0.0, self.height as f32 - 1.0);
        let x0 = x.floor() as u32;
        let y0 = y.floor() as u32;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;

        let texel = |tx: u32, ty: u32| self.pixels[(ty as usize) * (self.width as usize) + tx as usize];

        let p00 = texel(x0, y0);
        let p10 = texel(x1, y0);
        let p01 = texel(x0, y1);
        let p11 = texel(x1, y1);

        let mix_channel = |shift: u32| {
            let c00 = ((p00 >> shift) & 0xFF) as f32;
            let c10 = ((p10 >> shift) & 0xFF) as f32;
            let c01 = ((p01 >> shift) & 0xFF) as f32;
            let c11 = ((p11 >> shift) & 0xFF) as f32;
            let top = c00 + (c10 - c00) * fx;
            let bottom = c01 + (c11 - c01) * fx;
            (top + (bottom - top) * fy).round().clamp(0.0, 255.0) as u32
        };

        Premul(
            (mix_channel(24) << 24)
                | (mix_channel(16) << 16)
                | (mix_channel(8) << 8)
                | mix_channel(0),
        )
    }

    /// Builds from straight RGBA8, e.g. decoded PNG.
    pub fn from_rgba8(width: u32, height: u32, rgba: &[u8]) -> Self {
        let count = (width as usize) * (height as usize);
        let mut pixels = vec![0u32; count];
        for i in 0..count {
            let c = Color::rgba(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], rgba[i * 4 + 3]);
            pixels[i] = c.premultiply().0;
        }
        Self { width, height, pixels }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_sets_every_pixel() {
        let mut canvas = Canvas::new(4, 4);
        canvas.clear(Color::rgb(10, 20, 30).premultiply());
        assert_eq!(canvas.pixel(0, 0), Color::rgb(10, 20, 30).premultiply());
        assert_eq!(canvas.pixel(3, 3), Color::rgb(10, 20, 30).premultiply());
    }

    #[test]
    fn fill_rect_covers_exact_area() {
        let mut canvas = Canvas::new(10, 10);
        canvas.clear(Color::BLACK.premultiply());
        canvas.fill_rect(Rect::new(2.0, 2.0, 4.0, 4.0), Color::WHITE.premultiply(), canvas.bounds());
        assert_eq!(canvas.pixel(1, 1), Color::BLACK.premultiply());
        assert_eq!(canvas.pixel(2, 2), Color::WHITE.premultiply());
        assert_eq!(canvas.pixel(5, 5), Color::WHITE.premultiply());
        assert_eq!(canvas.pixel(6, 6), Color::BLACK.premultiply());
    }

    #[test]
    fn fill_rect_clips_to_clip_rect() {
        let mut canvas = Canvas::new(10, 10);
        canvas.clear(Color::BLACK.premultiply());
        let clip = Rect::new(0.0, 0.0, 5.0, 10.0);
        canvas.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE.premultiply(), clip);
        assert_eq!(canvas.pixel(4, 5), Color::WHITE.premultiply());
        assert_eq!(canvas.pixel(5, 5), Color::BLACK.premultiply());
    }

    #[test]
    fn fractional_edges_are_antialiased() {
        let mut canvas = Canvas::new(4, 1);
        canvas.clear(Color::BLACK.premultiply());
        canvas.fill_rect(Rect::new(1.5, 0.0, 1.0, 1.0), Color::WHITE.premultiply(), canvas.bounds());
        // Compositing over an opaque destination always yields alpha 255, so
        // coverage shows up in the colour channels instead.
        let left = canvas.pixel(1, 0);
        let right = canvas.pixel(2, 0);
        assert_eq!(left.r(), right.r(), "both half-covered pixels get the same blend");
        assert!((100..=160).contains(&left.r()), "half coverage expected, got r={}", left.r());
        assert_eq!(canvas.pixel(0, 0).r(), 0, "outside the rect stays black");
    }

    #[test]
    fn round_rect_clears_corners() {
        let mut canvas = Canvas::new(20, 20);
        canvas.clear(Color::TRANSPARENT.premultiply());
        canvas.fill_round_rect(Rect::new(0.0, 0.0, 20.0, 20.0), 8.0, Color::WHITE.premultiply(), canvas.bounds());
        assert_eq!(canvas.pixel(10, 10).a(), 255, "centre filled");
        assert_eq!(canvas.pixel(0, 0).a(), 0, "corner clipped by radius");
        assert_eq!(canvas.pixel(1, 1).a(), 0, "corner stays empty");
        let edge = canvas.pixel(2, 2).a();
        assert!(edge < 255, "corner edge is partially covered, got {edge}");
    }

    #[test]
    fn stroke_rect_leaves_interior_untouched() {
        let mut canvas = Canvas::new(20, 20);
        canvas.clear(Color::TRANSPARENT.premultiply());
        canvas.stroke_rect(Rect::new(2.0, 2.0, 16.0, 16.0), 0.0, 2.0, Color::WHITE.premultiply(), canvas.bounds());
        assert_eq!(canvas.pixel(10, 10).a(), 0, "interior untouched");
        assert!(canvas.pixel(2, 10).a() > 200, "left edge stroked");
        assert!(canvas.pixel(17, 10).a() > 200, "right edge stroked");
    }

    #[test]
    fn image_roundtrip_keeps_colour() {
        let rgba = vec![255u8, 0, 0, 255, 0, 255, 0, 255];
        let image = ImagePixels::from_rgba8(2, 1, &rgba);
        assert_eq!(image.sample_bilinear(0.0, 0.0).to_color(), Color::rgb(255, 0, 0));
        assert_eq!(image.sample_bilinear(1.0, 0.0).to_color(), Color::rgb(0, 255, 0));
    }

    #[test]
    fn image_scales_into_destination() {
        let rgba = vec![255u8; 4 * 4];
        let image = ImagePixels::from_rgba8(2, 2, &rgba);
        let mut canvas = Canvas::new(8, 8);
        canvas.clear(Color::TRANSPARENT.premultiply());
        canvas.draw_image(Rect::new(0.0, 0.0, 8.0, 8.0), &image, canvas.bounds());
        assert_eq!(canvas.pixel(4, 4).a(), 255);
    }

    #[test]
    fn export_produces_straight_alpha() {
        let mut canvas = Canvas::new(2, 1);
        canvas.clear(Color::rgba(255, 0, 0, 128).premultiply());
        let rgba = canvas.to_rgba8();
        assert_eq!(rgba[0], 255);
        assert_eq!(rgba[3], 128);
    }
}
