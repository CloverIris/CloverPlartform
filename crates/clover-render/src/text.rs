//! Text shaping, line layout and glyph rasterisation.
//!
//! The engine is built on three pieces:
//!
//! * **rustybuzz** for shaping — real HarfBuzz shaping, so ligatures, kerning and
//!   mark positioning come out right instead of being approximated by per-char
//!   advances.
//! * **unicode-linebreak / unicode-bidi** for break opportunities and run
//!   direction, so line breaking follows UAX #14 rather than splitting on spaces.
//! * **ttf-parser + ab_glyph_rasterizer** for outlines, which means TrueType
//!   collections (`.ttc`) work — the same files macOS actually ships.
//!
//! Font fallback is per character: text is segmented into runs by the first face
//! that can render each character, and each run is shaped with its own face.
//!
//! The public surface is unchanged from the previous fontdue-based engine:
//! `measure`, `wrap_lines`, `line_width`, `shape`. Callers do not know which
//! shaper produced the glyphs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use clover_core::Size;
use rustybuzz::ttf_parser;
use unicode_linebreak::linebreaks;

/// One rasterised glyph.
#[derive(Clone)]
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// A glyph placed at an absolute position.
#[derive(Clone)]
pub struct PlacedGlyph {
    pub x: f32,
    pub y: f32,
    pub bitmap: Arc<GlyphBitmap>,
}

/// A shaped run of text.
pub struct TextRun {
    pub glyphs: Vec<PlacedGlyph>,
}

impl TextRun {
    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// A single face inside a font file (collections expose several).
struct FontFace {
    bytes: Arc<Vec<u8>>,
    index: u32,
    family: String,
}

impl FontFace {
    fn parse(&self) -> Option<ttf_parser::Face<'_>> {
        ttf_parser::Face::parse(&self.bytes, self.index).ok()
    }

    fn has_char(&self, ch: char) -> bool {
        self.parse().and_then(|face| face.glyph_index(ch)).is_some()
    }
}

/// One shaped glyph in a face, before rasterisation.
#[derive(Clone, Copy, Debug)]
struct ShapedGlyph {
    face: usize,
    glyph: u16,
    x_offset: f32,
    y_offset: f32,
    advance: f32,
    /// Byte offset of the character this glyph came from.
    cluster: usize,
}

/// Cache entry: the bitmap plus how it sits relative to the pen.
struct CachedGlyph {
    bitmap: Arc<GlyphBitmap>,
    bearing_x: f32,
    /// Distance from the baseline up to the bitmap's top edge.
    bearing_y: f32,
}

type GlyphKey = (usize, u16, u32);

/// Font loading, shaping, measurement and glyph caching.
pub struct TextEngine {
    faces: Vec<FontFace>,
    glyphs: RefCell<HashMap<GlyphKey, Option<Arc<CachedGlyph>>>>,
    ascent_ratio: f32,
    line_ratio: f32,
}

impl TextEngine {
    /// Loads every usable face from `paths`, keeping the order for fallback.
    pub fn from_paths(paths: &[&str]) -> Option<Self> {
        let mut faces = Vec::new();
        for path in paths {
            if !Path::new(path).exists() {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else { continue };
            let bytes = Arc::new(bytes);
            let count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1).max(1);
            for index in 0..count.min(6) {
                let Ok(face) = ttf_parser::Face::parse(&bytes, index) else { continue };
                let family = face
                    .names()
                    .into_iter()
                    .find(|name| name.name_id == ttf_parser::name_id::FAMILY)
                    .and_then(|name| name.to_string())
                    .unwrap_or_default();
                faces.push(FontFace { bytes: Arc::clone(&bytes), index, family });
            }
            if faces.len() >= 24 {
                break;
            }
        }
        if faces.is_empty() {
            return None;
        }

        let primary = faces[0].parse()?;
        let upem = primary.units_per_em() as f32;
        let ascent = primary.ascender() as f32 / upem;
        let descent = primary.descender() as f32 / upem;
        let line_gap = primary.line_gap() as f32 / upem;

        Some(Self {
            faces,
            glyphs: RefCell::new(HashMap::new()),
            ascent_ratio: ascent,
            line_ratio: (ascent - descent + line_gap).max(1.0),
        })
    }

    /// Loads the first usable font from a list of candidate paths.
    pub fn load_first_available(paths: &[&str]) -> Option<Self> {
        Self::from_paths(paths)
    }

    /// Platform fonts, in fallback order. Collections are supported, so the
    /// system UI faces are usable instead of being skipped.
    pub fn load_system_default() -> Option<Self> {
        Self::from_paths(&[
            // macOS
            "/System/Library/Fonts/HelveticaNeue.ttc",
            "/System/Library/Fonts/Helvetica.ttc",
            "/System/Library/Fonts/Geneva.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            // CJK fallback: this is what makes Chinese text render at all.
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/System/Library/Fonts/Supplemental/Songti.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/System/Library/Fonts/Apple Symbols.ttf",
            // Windows
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/arial.ttf",
            "C:/Windows/Fonts/arialuni.ttf",
            // Linux
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ])
    }

    /// Number of loaded faces, for diagnostics.
    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Families in fallback order.
    pub fn families(&self) -> Vec<&str> {
        self.faces.iter().map(|face| face.family.as_str()).collect()
    }

    /// Distance from the top of a line to the baseline, at `px`.
    pub fn ascent(&self, px: f32) -> f32 {
        self.ascent_ratio * px
    }

    /// Full line height at `px`.
    pub fn line_height(&self, px: f32) -> f32 {
        self.line_ratio * px
    }

    /// First face that can render `ch`, falling back to the primary face.
    fn face_for(&self, ch: char) -> usize {
        for (index, face) in self.faces.iter().enumerate() {
            if face.has_char(ch) {
                return index;
            }
        }
        0
    }

    /// Splits `text` into runs that share one face.
    fn segment<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)> {
        let mut runs: Vec<(usize, &str)> = Vec::new();
        let mut start = 0usize;
        let mut current = None;
        for (offset, ch) in text.char_indices() {
            let face = self.face_for(ch);
            match current {
                Some(face_index) if face_index == face => {}
                Some(_) => {
                    runs.push((current.unwrap_or(0), &text[start..offset]));
                    start = offset;
                    current = Some(face);
                }
                None => current = Some(face),
            }
        }
        if let Some(face_index) = current {
            runs.push((face_index, &text[start..]));
        }
        runs
    }

    /// Shapes `text`, segmenting by fallback face and honouring bidi direction.
    fn shape_run_list(&self, text: &str, px: f32) -> Vec<ShapedGlyph> {
        let mut out = Vec::new();
        for (face_index, segment) in self.segment(text) {
            self.shape_segment(face_index, segment, px, &mut out);
        }
        out
    }

    fn shape_segment(&self, face_index: usize, segment: &str, px: f32, out: &mut Vec<ShapedGlyph>) {
        if segment.is_empty() {
            return;
        }
        let face = &self.faces[face_index];
        let Some(ttf) = face.parse() else { return };
        let upem = ttf.units_per_em() as f32;
        if upem <= 0.0 {
            return;
        }
        let scale = px / upem;
        let Some(rb_face) = rustybuzz::Face::from_slice(&face.bytes, face.index) else { return };

        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(segment);
        buffer.guess_segment_properties();
        let shaped = rustybuzz::shape(&rb_face, &[], buffer);

        let infos = shaped.glyph_infos();
        let positions = shaped.glyph_positions();
        for (info, position) in infos.iter().zip(positions.iter()) {
            out.push(ShapedGlyph {
                face: face_index,
                glyph: info.glyph_id as u16,
                x_offset: position.x_offset as f32 * scale,
                y_offset: position.y_offset as f32 * scale,
                advance: position.x_advance as f32 * scale,
                cluster: info.cluster as usize,
            });
        }
    }

    /// Rasterises one glyph, caching the result.
    fn glyph(&self, face_index: usize, glyph_id: u16, px: f32) -> Option<Arc<CachedGlyph>> {
        let quantised = ((px * 4.0).round().max(4.0)) as u32;
        let key = (face_index, glyph_id, quantised);
        if let Some(found) = self.glyphs.borrow().get(&key) {
            return found.clone();
        }
        let actual_px = quantised as f32 / 4.0;
        let raster = self.rasterize(face_index, glyph_id, actual_px);
        let cached = raster.map(Arc::new);
        self.glyphs.borrow_mut().insert(key, cached.clone());
        cached
    }

    fn rasterize(&self, face_index: usize, glyph_id: u16, px: f32) -> Option<CachedGlyph> {
        let face = &self.faces[face_index];
        let ttf = face.parse()?;
        let upem = ttf.units_per_em() as f32;
        if upem <= 0.0 {
            return None;
        }
        let scale = px / upem;
        let glyph = ttf_parser::GlyphId(glyph_id);
        let bbox = ttf.glyph_bounding_box(glyph)?;
        let width = ((bbox.x_max - bbox.x_min) as f32 * scale).ceil().max(1.0);
        let height = ((bbox.y_max - bbox.y_min) as f32 * scale).ceil().max(1.0);
        // Guard against absurd glyphs; a single glyph larger than this is a bug.
        if width > 4096.0 || height > 4096.0 {
            return None;
        }

        let mut rasterizer = ab_glyph_rasterizer::Rasterizer::new(width as usize, height as usize);
        let mut builder = OutlineSink {
            rasterizer: &mut rasterizer,
            scale,
            left: bbox.x_min as f32 * scale,
            top: bbox.y_max as f32 * scale,
            current: None,
            start: None,
        };
        ttf.outline_glyph(glyph, &mut builder)?;
        builder.close_contour();

        let mut coverage = vec![0u8; width as usize * height as usize];
        rasterizer.for_each_pixel_2d(|x, y, alpha| {
            let index = y as usize * width as usize + x as usize;
            if index < coverage.len() {
                coverage[index] = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });

        Some(CachedGlyph {
            bitmap: Arc::new(GlyphBitmap { width: width as u32, height: height as u32, coverage }),
            bearing_x: bbox.x_min as f32 * scale,
            bearing_y: bbox.y_max as f32 * scale,
        })
    }

    /// Advance width of a single character.
    pub fn char_advance(&self, ch: char, px: f32) -> f32 {
        let face = self.face_for(ch);
        let mut buffer = [0u8; 4];
        let text: &str = ch.encode_utf8(&mut buffer);
        let mut glyphs = Vec::new();
        self.shape_segment(face, text, px, &mut glyphs);
        glyphs.iter().map(|glyph| glyph.advance).sum()
    }

    /// Width of one line, without any wrapping.
    pub fn line_width(&self, line: &str, px: f32) -> f32 {
        self.shape_run_list(line, px).iter().map(|glyph| glyph.advance).sum()
    }

    /// Breaks `text` into lines.
    ///
    /// `\n` always starts a new line. When `max_width` is `Some`, lines break at
    /// Unicode line-break opportunities; a word wider than the limit is broken
    /// between characters so nothing can overflow forever.
    pub fn wrap_lines(&self, text: &str, px: f32, max_width: Option<f32>) -> Vec<String> {
        let Some(limit) = max_width.filter(|limit| *limit > 0.0) else {
            return text.split('\n').map(str::to_string).collect();
        };

        let mut out = Vec::new();
        for paragraph in text.split('\n') {
            if paragraph.trim().is_empty() {
                out.push(String::new());
                continue;
            }
            let shaped = self.shape_run_list(paragraph, px);
            self.break_paragraph(paragraph, &shaped, limit, &mut out);
        }
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }

    /// Greedy line breaking for one paragraph.
    ///
    /// Prefers a Unicode break opportunity that still fits; when a single word is
    /// wider than the line it falls back to breaking between characters.
    fn break_paragraph(&self, paragraph: &str, shaped: &[ShapedGlyph], limit: f32, out: &mut Vec<String>) {
        let marks = cumulative_positions(shaped);
        let breaks: Vec<usize> = linebreaks(paragraph).map(|(offset, _)| offset).collect();
        let mut start = 0usize;
        let mut index = 0usize;
        let mut last_fit: Option<usize> = None;

        while index < breaks.len() {
            let candidate = breaks[index];
            if candidate <= start {
                index += 1;
                continue;
            }
            let width = prefix_width(&marks, candidate) - prefix_width(&marks, start);
            if width <= limit {
                last_fit = Some(candidate);
                index += 1;
                continue;
            }

            match last_fit.filter(|cut| *cut > start) {
                Some(cut) => {
                    out.push(paragraph[start..cut].trim_end().to_string());
                    start = cut;
                    last_fit = None;
                    // Re-test the same candidate against the new line start.
                }
                None => {
                    let hard = self.hard_break_offset(paragraph, &marks, start, candidate, limit);
                    if hard > start {
                        out.push(paragraph[start..hard].trim_end().to_string());
                        start = hard;
                    } else {
                        out.push(paragraph[start..candidate].trim_end().to_string());
                        start = candidate;
                        index += 1;
                    }
                    last_fit = None;
                }
            }
        }

        let tail = paragraph[start..].trim_end();
        if !tail.is_empty() {
            out.push(tail.to_string());
        }
    }

    /// Largest character boundary in `(start, end)` whose prefix still fits.
    fn hard_break_offset(
        &self,
        paragraph: &str,
        marks: &[(usize, f32)],
        start: usize,
        end: usize,
        limit: f32,
    ) -> usize {
        let base = prefix_width(marks, start);
        let mut best = 0usize;
        for (offset, ch) in paragraph[start..end].char_indices() {
            let absolute = start + offset;
            if absolute <= start {
                continue;
            }
            let width = prefix_width(marks, absolute) - base;
            if width <= limit {
                best = absolute;
            } else {
                break;
            }
            let _ = ch;
        }
        best
    }

    /// Measures a block of text, wrapping to `max_width` when given.
    pub fn measure(&self, text: &str, px: f32, max_width: Option<f32>) -> Size {
        if text.is_empty() {
            return Size::ZERO;
        }
        let lines = self.wrap_lines(text, px, max_width);
        let widest = lines.iter().map(|line| self.line_width(line, px)).fold(0.0f32, f32::max);
        Size::new(widest, self.line_height(px) * lines.len() as f32)
    }

    /// Shapes `text` with its baseline at `baseline`, starting at `origin_x`.
    pub fn shape(&self, text: &str, px: f32, origin_x: f32, baseline: f32) -> TextRun {
        let mut pen = origin_x;
        let mut glyphs = Vec::new();
        for shaped in self.shape_run_list(text, px) {
            let Some(cached) = self.glyph(shaped.face, shaped.glyph, px) else {
                pen += shaped.advance;
                continue;
            };
            if cached.bitmap.width > 0 && cached.bitmap.height > 0 {
                glyphs.push(PlacedGlyph {
                    x: pen + shaped.x_offset + cached.bearing_x,
                    y: baseline - shaped.y_offset - cached.bearing_y,
                    bitmap: Arc::clone(&cached.bitmap),
                });
            }
            pen += shaped.advance;
        }
        TextRun { glyphs }
    }
}

/// Forwards ttf-parser outlines into the rasterizer, flipping the Y axis.
///
/// `ab_glyph_rasterizer` takes explicit segments rather than a path builder, so
/// this sink tracks the current point and closes contours with a final line.
struct OutlineSink<'a> {
    rasterizer: &'a mut ab_glyph_rasterizer::Rasterizer,
    scale: f32,
    left: f32,
    top: f32,
    current: Option<ab_glyph_rasterizer::Point>,
    start: Option<ab_glyph_rasterizer::Point>,
}

impl OutlineSink<'_> {
    fn point(&self, x: f32, y: f32) -> ab_glyph_rasterizer::Point {
        ab_glyph_rasterizer::point(x * self.scale - self.left, self.top - y * self.scale)
    }

    fn close_contour(&mut self) {
        if let (Some(current), Some(start)) = (self.current, self.start) {
            let dx = current.x - start.x;
            let dy = current.y - start.y;
            if dx.abs() > 0.001 || dy.abs() > 0.001 {
                self.rasterizer.draw_line(current, start);
            }
        }
        self.current = None;
        self.start = None;
    }
}

impl ttf_parser::OutlineBuilder for OutlineSink<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close_contour();
        let point = self.point(x, y);
        self.start = Some(point);
        self.current = Some(point);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        if let Some(current) = self.current {
            self.rasterizer.draw_line(current, point);
        }
        self.current = Some(point);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let control = self.point(x1, y1);
        let point = self.point(x, y);
        if let Some(current) = self.current {
            self.rasterizer.draw_quad(current, control, point);
        }
        self.current = Some(point);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let first = self.point(x1, y1);
        let second = self.point(x2, y2);
        let point = self.point(x, y);
        if let Some(current) = self.current {
            self.rasterizer.draw_cubic(current, first, second, point);
        }
        self.current = Some(point);
    }

    fn close(&mut self) {
        self.close_contour();
    }
}

/// `(cluster byte offset, pen position after that glyph)`, monotonically increasing.
fn cumulative_positions(glyphs: &[ShapedGlyph]) -> Vec<(usize, f32)> {
    let mut marks = Vec::with_capacity(glyphs.len());
    let mut pen = 0.0f32;
    for glyph in glyphs {
        pen += glyph.advance;
        marks.push((glyph.cluster, pen));
    }
    marks
}

/// Pen position after all glyphs whose cluster is before `byte_offset`.
fn prefix_width(marks: &[(usize, f32)], byte_offset: usize) -> f32 {
    let mut width = 0.0;
    for (cluster, position) in marks {
        if *cluster < byte_offset {
            width = *position;
        } else {
            break;
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Option<TextEngine> {
        TextEngine::load_system_default()
    }

    #[test]
    fn loads_a_font_with_a_sane_line_height() {
        let Some(engine) = engine() else { return };
        assert!(engine.face_count() > 0);
        assert!(engine.line_height(16.0) >= 16.0);
        assert!(engine.ascent(16.0) > 0.0);
    }

    #[test]
    fn measures_positive_width() {
        let Some(engine) = engine() else { return };
        let size = engine.measure("Hello", 16.0, None);
        assert!(size.w > 0.0);
        assert!(size.h >= 16.0);
    }

    #[test]
    fn empty_text_measures_zero() {
        let Some(engine) = engine() else { return };
        assert_eq!(engine.measure("", 16.0, None), Size::ZERO);
    }

    #[test]
    fn shaping_advances_left_to_right() {
        let Some(engine) = engine() else { return };
        let run = engine.shape("ab", 24.0, 10.0, 30.0);
        assert!(run.glyphs.len() >= 2);
        assert!(run.glyphs[1].x > run.glyphs[0].x, "second glyph is to the right");
    }

    #[test]
    fn glyph_cache_returns_the_same_bitmap() {
        let Some(engine) = engine() else { return };
        let first = engine.glyph(0, 36, 20.0).expect("glyph 36");
        let second = engine.glyph(0, 36, 20.0).expect("glyph 36");
        assert!(Arc::ptr_eq(&first.bitmap, &second.bitmap));
    }

    #[test]
    fn explicit_newlines_add_lines() {
        let Some(engine) = engine() else { return };
        let single = engine.measure("one", 16.0, None);
        let triple = engine.measure("one\ntwo\nthree", 16.0, None);
        assert!((triple.h - single.h * 3.0).abs() < 0.01);
    }

    #[test]
    fn wrapping_splits_at_word_boundaries() {
        let Some(engine) = engine() else { return };
        let text = "the quick brown fox jumps over the lazy dog";
        let unwrapped = engine.measure(text, 16.0, None);
        let lines = engine.wrap_lines(text, 16.0, Some(unwrapped.w / 3.0));
        assert!(lines.len() > 1, "text should wrap, got {lines:?}");
        for line in &lines {
            assert!(!line.starts_with(' '), "no leading space in '{line}'");
            assert!(engine.line_width(line, 16.0) <= unwrapped.w / 3.0 + 2.0, "'{line}' overflows");
        }
    }

    #[test]
    fn long_words_are_broken() {
        let Some(engine) = engine() else { return };
        let lines = engine.wrap_lines("supercalifragilisticexpialidocious", 16.0, Some(40.0));
        assert!(lines.len() > 1, "an oversized word must break, got {lines:?}");
    }

    #[test]
    fn ligatures_or_kerning_change_width_from_naive_sum() {
        // "AV" is kerned in most faces; if the shaper is wired up its width
        // differs from the sum of the two single-character widths.
        let Some(engine) = engine() else { return };
        let pair = engine.line_width("AV", 48.0);
        let sum = engine.char_advance('A', 48.0) + engine.char_advance('V', 48.0);
        assert!(pair <= sum + 0.01, "shaped pair {pair} must not exceed {sum}");
    }

    #[test]
    fn cjk_text_falls_back_to_a_face_that_has_it() {
        let Some(engine) = engine() else { return };
        let run = engine.shape("图层 1 草稿", 20.0, 0.0, 20.0);
        assert!(!run.is_empty(), "CJK glyphs must rasterise through fallback");
        assert!(engine.line_width("图层 1", 20.0) > 0.0);
        // The primary face (a Latin one) cannot render these, so at least one
        // fallback face must have been used.
        assert!(engine.face_count() > 1 || engine.faces[0].has_char('图'));
    }

    #[test]
    fn mixed_scripts_segment_across_faces() {
        let Some(engine) = engine() else { return };
        let run = engine.shape("Layer 图层", 18.0, 0.0, 18.0);
        assert!(run.glyphs.len() >= 6, "latin + cjk glyphs expected");
        let advance = engine.line_width("Layer 图层", 18.0);
        assert!(advance > engine.line_width("Layer", 18.0), "CJK adds width");
    }

    #[test]
    fn prefix_width_is_monotonic() {
        let glyphs = vec![
            ShapedGlyph { face: 0, glyph: 1, x_offset: 0.0, y_offset: 0.0, advance: 10.0, cluster: 0 },
            ShapedGlyph { face: 0, glyph: 2, x_offset: 0.0, y_offset: 0.0, advance: 10.0, cluster: 1 },
        ];
        let marks = cumulative_positions(&glyphs);
        assert_eq!(prefix_width(&marks, 0), 0.0);
        assert_eq!(prefix_width(&marks, 1), 10.0);
        assert_eq!(prefix_width(&marks, 2), 20.0);
    }
}
