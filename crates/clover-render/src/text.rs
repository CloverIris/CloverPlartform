//! Text shaping and glyph rasterisation.
//!
//! Glyphs are rasterised **once** into coverage bitmaps that the renderer can
//! blit from any thread. All shaping happens while the display list is built, so
//! the tile renderer never touches font state — that is what makes parallel
//! rasterisation safe without locks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use clover_core::Size;
use fontdue::{Font, FontSettings};

/// One rasterised glyph.
#[derive(Clone)]
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    /// Horizontal offset from the pen position to the bitmap's left edge.
    pub bearing_x: i32,
    /// Vertical offset from the baseline to the bitmap's top edge.
    pub bearing_y: i32,
    pub advance: f32,
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

type GlyphKey = (char, u32);

/// Font loading, measurement and glyph caching.
pub struct TextEngine {
    font: Font,
    glyphs: RefCell<HashMap<GlyphKey, Arc<GlyphBitmap>>>,
    ascent_ratio: f32,
    line_ratio: f32,
}

impl TextEngine {
    /// Builds an engine from raw font bytes (TTF; TTC is not supported).
    pub fn from_bytes(data: Vec<u8>) -> Option<Self> {
        let font = Font::from_bytes(data, FontSettings::default()).ok()?;
        let line = font.horizontal_line_metrics(16.0)?;
        let ascent_ratio = line.ascent / 16.0;
        let line_ratio = (line.ascent - line.descent + line.line_gap) / 16.0;
        Some(Self {
            font,
            glyphs: RefCell::new(HashMap::new()),
            ascent_ratio,
            line_ratio: line_ratio.max(1.0),
        })
    }

    /// Loads the first usable font from a list of candidate paths.
    pub fn load_first_available(paths: &[&str]) -> Option<Self> {
        for path in paths {
            if !Path::new(path).exists() {
                continue;
            }
            if let Ok(bytes) = std::fs::read(path) {
                if let Some(engine) = Self::from_bytes(bytes) {
                    return Some(engine);
                }
            }
        }
        None
    }

    /// Platform-appropriate candidate fonts with scalable TrueType outlines.
    ///
    /// TTC files are skipped because fontdue reads a single face.
    pub fn load_system_default() -> Option<Self> {
        Self::load_first_available(&[
            // macOS
            "/System/Library/Fonts/Geneva.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Apple Symbols.ttf",
            // Windows
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/arial.ttf",
            // Linux
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ])
    }

    /// Distance from the top of a line to the baseline, at `px`.
    pub fn ascent(&self, px: f32) -> f32 {
        self.ascent_ratio * px
    }

    /// Full line height at `px`.
    pub fn line_height(&self, px: f32) -> f32 {
        self.line_ratio * px
    }

    fn glyph(&self, ch: char, px: f32) -> Arc<GlyphBitmap> {
        let quantised = ((px * 4.0).round().max(4.0)) as u32;
        let key = (ch, quantised);
        if let Some(found) = self.glyphs.borrow().get(&key) {
            return Arc::clone(found);
        }
        let actual_px = quantised as f32 / 4.0;
        let (metrics, coverage) = self.font.rasterize(ch, actual_px);
        let bitmap = Arc::new(GlyphBitmap {
            width: metrics.width as u32,
            height: metrics.height as u32,
            bearing_x: metrics.xmin,
            bearing_y: metrics.ymin + metrics.height as i32,
            advance: metrics.advance_width,
            coverage,
        });
        self.glyphs.borrow_mut().insert(key, Arc::clone(&bitmap));
        bitmap
    }

    /// Advance width of a single character.
    pub fn char_advance(&self, ch: char, px: f32) -> f32 {
        self.glyph(ch, px).advance
    }

    /// Width of one line, without any wrapping.
    pub fn line_width(&self, line: &str, px: f32) -> f32 {
        line.chars().map(|ch| self.glyph(ch, px).advance).sum()
    }

    /// Breaks `text` into lines.
    ///
    /// `\n` always starts a new line. When `max_width` is `Some`, lines are
    /// additionally word-wrapped to that width; words longer than the width are
    /// hard-broken so nothing can overflow indefinitely.
    pub fn wrap_lines(&self, text: &str, px: f32, max_width: Option<f32>) -> Vec<String> {
        let Some(limit) = max_width.filter(|l| *l > 0.0) else {
            return text.split('\n').map(str::to_string).collect();
        };
        let mut out = Vec::new();
        for paragraph in text.split('\n') {
            if paragraph.trim().is_empty() {
                out.push(String::new());
                continue;
            }
            self.wrap_paragraph(paragraph, px, limit, &mut out);
        }
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }

    fn wrap_paragraph(&self, paragraph: &str, px: f32, limit: f32, out: &mut Vec<String>) {
        let space_w = self.char_advance(' ', px);
        let mut line = String::new();
        let mut line_w = 0.0f32;

        for word in paragraph.split_whitespace() {
            let word_w = self.line_width(word, px);

            // Start a new line when the word does not fit.
            if !line.is_empty() && line_w + space_w + word_w > limit {
                out.push(std::mem::take(&mut line));
                line_w = 0.0;
            }

            // A word wider than the whole line is broken mid-word.
            if line.is_empty() && word_w > limit {
                let mut chunk = String::new();
                let mut chunk_w = 0.0f32;
                for ch in word.chars() {
                    let advance = self.char_advance(ch, px);
                    if chunk_w + advance > limit && !chunk.is_empty() {
                        out.push(std::mem::take(&mut chunk));
                        chunk_w = 0.0;
                    }
                    chunk.push(ch);
                    chunk_w += advance;
                }
                line = chunk;
                line_w = chunk_w;
                continue;
            }

            if !line.is_empty() {
                line.push(' ');
                line_w += space_w;
            }
            line.push_str(word);
            line_w += word_w;
        }
        out.push(line);
    }

    /// Measures a block of text, wrapping to `max_width` when given.
    pub fn measure(&self, text: &str, px: f32, max_width: Option<f32>) -> Size {
        if text.is_empty() {
            return Size::ZERO;
        }
        let lines = self.wrap_lines(text, px, max_width);
        let widest = lines.iter().map(|l| self.line_width(l, px)).fold(0.0f32, f32::max);
        Size::new(widest, self.line_height(px) * lines.len() as f32)
    }

    /// Shapes `text` with its baseline at `baseline`, starting at `origin_x`.
    pub fn shape(&self, text: &str, px: f32, origin_x: f32, baseline: f32) -> TextRun {
        let mut pen = origin_x;
        let mut glyphs = Vec::with_capacity(text.len());
        for ch in text.chars() {
            if ch == '\n' {
                continue;
            }
            let bitmap = self.glyph(ch, px);
            glyphs.push(PlacedGlyph {
                x: pen + bitmap.bearing_x as f32,
                y: baseline - bitmap.bearing_y as f32,
                bitmap: Arc::clone(&bitmap),
            });
            pen += bitmap.advance;
        }
        TextRun { glyphs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Option<TextEngine> {
        TextEngine::load_system_default()
    }

    #[test]
    fn measures_positive_width() {
        let Some(engine) = engine() else {
            eprintln!("no system font available, skipping");
            return;
        };
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
        assert_eq!(run.glyphs.len(), 2);
        assert!(run.glyphs[1].x > run.glyphs[0].x, "second glyph must be to the right");
    }

    #[test]
    fn glyph_cache_returns_same_arc() {
        let Some(engine) = engine() else { return };
        let a = engine.glyph('A', 20.0);
        let b = engine.glyph('A', 20.0);
        assert!(Arc::ptr_eq(&a, &b), "same glyph must hit the cache");
    }

    #[test]
    fn explicit_newlines_add_lines() {
        let Some(engine) = engine() else { return };
        let single = engine.measure("one", 16.0, None);
        let triple = engine.measure("one\ntwo\nthree", 16.0, None);
        assert!((triple.h - single.h * 3.0).abs() < 0.01, "three lines tall");
    }

    #[test]
    fn wrapping_splits_on_spaces() {
        let Some(engine) = engine() else { return };
        let text = "the quick brown fox jumps over the lazy dog";
        let unwrapped = engine.measure(text, 16.0, None);
        let wrapped = engine.measure(text, 16.0, Some(unwrapped.w / 3.0));
        assert!(wrapped.h > unwrapped.h, "wrapped text needs more lines");
        assert!(wrapped.w <= unwrapped.w / 3.0 + 1.0, "no line exceeds the limit");
    }

    #[test]
    fn long_words_are_hard_broken() {
        let Some(engine) = engine() else { return };
        let lines = engine.wrap_lines("supercalifragilisticexpialidocious", 16.0, Some(40.0));
        assert!(lines.len() > 1, "an oversized word must break, got {lines:?}");
        for line in &lines {
            assert!(engine.line_width(line, 16.0) <= 41.0, "line '{line}' overflowed");
        }
    }

    #[test]
    fn wrapping_keeps_explicit_lines() {
        let Some(engine) = engine() else { return };
        let lines = engine.wrap_lines("a\nb", 16.0, Some(500.0));
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn larger_size_has_larger_line_height() {
        let Some(engine) = engine() else { return };
        assert!(engine.line_height(32.0) > engine.line_height(16.0));
        assert!(engine.ascent(32.0) > engine.ascent(16.0));
    }
}
