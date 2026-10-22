//! Render context and surface format negotiation.
//!
//! The context is the handle a future GPU backend would take: it owns the target
//! description (format, colour space, scale, viewport) and later the command
//! recording state. The CPU renderer is simply one consumer of it.
//!
//! Format negotiation is explicit rather than implicit: the host states what it
//! wants, the framework states what the display can do, and the result records
//! *why* it fell back. Silent degradation is how HDR bugs survive to production.

use clover_core::Size;

/// Pixel format of a presented surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceFormat {
    /// Premultiplied BGRA8, sRGB transfer. The universal fallback.
    Bgra8Sdr,
    /// Premultiplied linear RGBA, 16-bit float. Wide gamut and HDR capable.
    Rgba16Float,
}

impl SurfaceFormat {
    /// Bytes per pixel.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            SurfaceFormat::Bgra8Sdr => 4,
            SurfaceFormat::Rgba16Float => 8,
        }
    }

    pub fn is_hdr(self) -> bool {
        matches!(self, SurfaceFormat::Rgba16Float)
    }
}

/// Colour primaries + transfer function of a surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    /// sRGB primaries, sRGB transfer.
    Srgb,
    /// Display P3 primaries, sRGB transfer.
    DisplayP3,
    /// Rec.2020 primaries, linear or PQ transfer.
    Rec2020,
}

/// What the display can actually show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayCapabilities {
    /// The display accepts an extended-range signal.
    pub hdr: bool,
    /// The display covers more than sRGB.
    pub wide_gamut: bool,
}

/// What the host asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatRequest {
    pub hdr: bool,
    pub wide_gamut: bool,
}

/// Why a request was downgraded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// The host asked for HDR but the display does not accept it.
    DisplayHasNoHdr,
    /// The host asked for a wide gamut but the display is sRGB only.
    DisplayHasNoWideGamut,
    /// No HDR raster path is built into this renderer yet.
    HdrRasterUnavailable,
}

/// The negotiated result, including the reason when it is not what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatChoice {
    pub format: SurfaceFormat,
    pub color_space: ColorSpace,
    pub fallback: Option<FallbackReason>,
}

impl Default for FormatChoice {
    fn default() -> Self {
        Self { format: SurfaceFormat::Bgra8Sdr, color_space: ColorSpace::Srgb, fallback: None }
    }
}

impl FormatChoice {
    /// Negotiates the best format this build can actually render.
    ///
    /// `hdr_raster` is false until the float raster path lands; the negotiation
    /// records that as an explicit fallback instead of silently pretending.
    pub fn negotiate(
        request: FormatRequest,
        display: DisplayCapabilities,
        hdr_raster: bool,
    ) -> FormatChoice {
        if !request.hdr && !request.wide_gamut {
            return FormatChoice::default();
        }

        let mut fallback = None;
        if request.hdr {
            if !display.hdr {
                fallback = Some(FallbackReason::DisplayHasNoHdr);
            } else if !hdr_raster {
                fallback = Some(FallbackReason::HdrRasterUnavailable);
            } else {
                return FormatChoice {
                    format: SurfaceFormat::Rgba16Float,
                    color_space: ColorSpace::Rec2020,
                    fallback: None,
                };
            }
        }

        if request.wide_gamut && display.wide_gamut && fallback.is_none() {
            return FormatChoice {
                format: SurfaceFormat::Bgra8Sdr,
                color_space: ColorSpace::DisplayP3,
                fallback: None,
            };
        }
        if request.wide_gamut && !display.wide_gamut && fallback.is_none() {
            fallback = Some(FallbackReason::DisplayHasNoWideGamut);
        }

        FormatChoice {
            format: SurfaceFormat::Bgra8Sdr,
            color_space: ColorSpace::Srgb,
            fallback,
        }
    }
}

/// Everything a renderer needs to know about its target.
#[derive(Clone, Copy, Debug)]
pub struct RenderContext {
    pub format: SurfaceFormat,
    pub color_space: ColorSpace,
    pub scale: f32,
    pub viewport: Size,
    /// Set when the negotiated format is not what the host asked for.
    pub fallback: Option<FallbackReason>,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            format: SurfaceFormat::Bgra8Sdr,
            color_space: ColorSpace::Srgb,
            scale: 1.0,
            viewport: Size::ZERO,
            fallback: None,
        }
    }
}

impl RenderContext {
    pub fn new(choice: FormatChoice, viewport: Size, scale: f32) -> Self {
        Self {
            format: choice.format,
            color_space: choice.color_space,
            scale: scale.max(0.01),
            viewport,
            fallback: choice.fallback,
        }
    }

    /// Viewport in physical pixels.
    pub fn physical_size(&self) -> (u32, u32) {
        (
            (self.viewport.w * self.scale).round().max(1.0) as u32,
            (self.viewport.h * self.scale).round().max(1.0) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_request_stays_sdr() {
        let choice = FormatChoice::negotiate(FormatRequest::default(), DisplayCapabilities::default(), true);
        assert_eq!(choice.format, SurfaceFormat::Bgra8Sdr);
        assert_eq!(choice.color_space, ColorSpace::Srgb);
        assert_eq!(choice.fallback, None);
    }

    #[test]
    fn hdr_on_hdr_display_with_float_raster_gives_hdr() {
        let choice = FormatChoice::negotiate(
            FormatRequest { hdr: true, wide_gamut: false },
            DisplayCapabilities { hdr: true, wide_gamut: true },
            true,
        );
        assert_eq!(choice.format, SurfaceFormat::Rgba16Float);
        assert_eq!(choice.color_space, ColorSpace::Rec2020);
        assert_eq!(choice.fallback, None);
    }

    #[test]
    fn hdr_without_display_support_falls_back_and_says_why() {
        let choice = FormatChoice::negotiate(
            FormatRequest { hdr: true, wide_gamut: false },
            DisplayCapabilities::default(),
            true,
        );
        assert_eq!(choice.format, SurfaceFormat::Bgra8Sdr);
        assert_eq!(choice.fallback, Some(FallbackReason::DisplayHasNoHdr));
    }

    #[test]
    fn hdr_without_raster_path_falls_back_and_says_why() {
        let choice = FormatChoice::negotiate(
            FormatRequest { hdr: true, wide_gamut: false },
            DisplayCapabilities { hdr: true, wide_gamut: true },
            false,
        );
        assert_eq!(choice.format, SurfaceFormat::Bgra8Sdr);
        assert_eq!(choice.fallback, Some(FallbackReason::HdrRasterUnavailable));
    }

    #[test]
    fn wide_gamut_only_upgrades_the_colour_space() {
        let choice = FormatChoice::negotiate(
            FormatRequest { hdr: false, wide_gamut: true },
            DisplayCapabilities { hdr: false, wide_gamut: true },
            true,
        );
        assert_eq!(choice.format, SurfaceFormat::Bgra8Sdr);
        assert_eq!(choice.color_space, ColorSpace::DisplayP3);
        assert_eq!(choice.fallback, None);
    }

    #[test]
    fn physical_size_uses_the_scale() {
        let context = RenderContext::new(FormatChoice::default(), Size::new(400.0, 300.0), 2.0);
        assert_eq!(context.physical_size(), (800, 600));
    }

    #[test]
    fn bytes_per_pixel_matches_the_format() {
        assert_eq!(SurfaceFormat::Bgra8Sdr.bytes_per_pixel(), 4);
        assert_eq!(SurfaceFormat::Rgba16Float.bytes_per_pixel(), 8);
    }
}
