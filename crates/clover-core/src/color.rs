//! Colour and blending.
//!
//! Two representations coexist on purpose:
//!
//! * [`Color`] is straight (non-premultiplied) RGBA8, which is what UI
//!   description code and the public API use.
//! * [`Premul`] is premultiplied BGRA packed into a `u32`, which is what the
//!   rasteriser stores and blends. Premultiplied source-over is a single
//!   multiply-add per channel and is associative, so tiles can be rendered in
//!   any order and in parallel.

/// Straight RGBA8 colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Default for Color {
    fn default() -> Self {
        Color::TRANSPARENT
    }
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Parses `0xRRGGBBAA`, matching the CloverPic core convention.
    pub const fn from_rgba_u32(value: u32) -> Self {
        Self {
            r: ((value >> 24) & 0xFF) as u8,
            g: ((value >> 16) & 0xFF) as u8,
            b: ((value >> 8) & 0xFF) as u8,
            a: (value & 0xFF) as u8,
        }
    }

    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Same colour, alpha multiplied by `factor` in `[0, 1]`.
    pub fn scale_alpha(self, factor: f32) -> Self {
        let a = (self.a as f32 * factor.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8;
        self.with_alpha(a)
    }

    pub fn is_transparent(self) -> bool {
        self.a == 0
    }

    /// Linear interpolation in straight space, used for hover/press tints.
    pub fn lerp(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| {
            let a = a as f32;
            let b = b as f32;
            (a + (b - a) * t).round().clamp(0.0, 255.0) as u8
        };
        Color {
            r: mix(self.r, other.r),
            g: mix(self.g, other.g),
            b: mix(self.b, other.b),
            a: mix(self.a, other.a),
        }
    }

    /// Converts to the rasteriser's premultiplied BGRA word.
    pub const fn premultiply(self) -> Premul {
        let a = self.a as u32;
        let pr = (self.r as u32 * a / 255) as u8;
        let pg = (self.g as u32 * a / 255) as u8;
        let pb = (self.b as u32 * a / 255) as u8;
        Premul(((self.a as u32) << 24) | ((pr as u32) << 16) | ((pg as u32) << 8) | pb as u32)
    }
}

/// Premultiplied BGRA8 packed as `0xAARRGGBB` in a `u32` (little-endian memory
/// order is B, G, R, A — the layout CloverPic's BGRA8 frame expects).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Premul(pub u32);

impl Premul {
    pub const TRANSPARENT: Premul = Premul(0);

    #[inline]
    pub const fn a(self) -> u8 {
        ((self.0 >> 24) & 0xFF) as u8
    }

    #[inline]
    pub const fn r(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }

    #[inline]
    pub const fn g(self) -> u8 {
        ((self.0 >> 8) & 0xFF) as u8
    }

    #[inline]
    pub const fn b(self) -> u8 {
        (self.0 & 0xFF) as u8
    }

    #[inline]
    pub const fn from_channels(a: u8, r: u8, g: u8, b: u8) -> Self {
        Premul(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }

    /// Multiplies every channel by an 8-bit coverage mask, used for antialiasing
    /// and glyph coverage.
    #[inline]
    pub fn with_coverage(self, coverage: u8) -> Self {
        if coverage == 255 {
            return self;
        }
        if coverage == 0 {
            return Premul::TRANSPARENT;
        }
        let c = coverage as u32;
        let scale = |shift: u32| (((self.0 >> shift) & 0xFF) * c / 255) & 0xFF;
        Premul(
            (scale(24) << 24) | (scale(16) << 16) | (scale(8) << 8) | scale(0),
        )
    }

    /// Premultiplied source-over: `self` over `dst`.
    #[inline]
    pub fn over(self, dst: Premul) -> Premul {
        if self.a() == 255 {
            return self;
        }
        if self.a() == 0 {
            return dst;
        }
        let inv = 255 - self.a() as u32;
        let ch = |shift: u32| {
            let s = (self.0 >> shift) & 0xFF;
            let d = (dst.0 >> shift) & 0xFF;
            (s + d * inv / 255).min(255)
        };
        Premul((ch(24) << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0))
    }

    /// Un-premultiplies back to a straight [`Color`], for export.
    pub fn to_color(self) -> Color {
        let a = self.a();
        if a == 0 {
            return Color::TRANSPARENT;
        }
        let a32 = a as u32;
        let un = |v: u8| ((v as u32 * 255 + a32 / 2) / a32).min(255) as u8;
        Color::rgba(un(self.r()), un(self.g()), un(self.b()), a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_roundtrip_for_opaque() {
        let c = Color::rgb(10, 20, 30);
        assert_eq!(c.premultiply().to_color(), c);
    }

    #[test]
    fn premultiply_roundtrip_is_close_for_translucent() {
        let c = Color::rgba(200, 100, 50, 128);
        let back = c.premultiply().to_color();
        assert!((back.r as i32 - 200).abs() <= 1);
        assert!((back.g as i32 - 100).abs() <= 1);
        assert!((back.b as i32 - 50).abs() <= 1);
        assert_eq!(back.a, 128);
    }

    #[test]
    fn over_opaque_source_replaces_destination() {
        let src = Color::rgb(255, 0, 0).premultiply();
        let dst = Color::rgb(0, 0, 255).premultiply();
        assert_eq!(src.over(dst), src);
    }

    #[test]
    fn over_transparent_source_keeps_destination() {
        let src = Premul::TRANSPARENT;
        let dst = Color::rgb(1, 2, 3).premultiply();
        assert_eq!(src.over(dst), dst);
    }

    #[test]
    fn coverage_scales_channels() {
        let c = Color::rgb(255, 0, 0).premultiply();
        let half = c.with_coverage(128);
        assert!((half.a() as i32 - 128).abs() <= 1);
        assert!((half.r() as i32 - 128).abs() <= 1);
        assert_eq!(half.g(), 0);
    }

    #[test]
    fn from_rgba_u32_matches_cloverpic_order() {
        assert_eq!(Color::from_rgba_u32(0x11223344), Color::rgba(0x11, 0x22, 0x33, 0x44));
    }
}
