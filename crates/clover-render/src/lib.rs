//! CloverPlatform rendering layer.
//!
//! Pipeline: `UiTree` → [`display_list`] → tile-parallel [`renderer`] → [`canvas`].
//!
//! Everything runs on the CPU. The design goal is that a 4K frame of static UI
//! costs nothing (damage-driven), and a live canvas repaint costs only the tiles
//! that changed, spread across cores.

pub mod canvas;
pub mod display_list;
pub mod image;
pub mod renderer;
pub mod surface;
pub mod text;

pub use canvas::{Canvas, ImagePixels};
pub use display_list::{build, DisplayList, DrawCmd};
pub use image::{decode_png_bytes, decode_png_file, ImageStore};
pub use renderer::CpuRenderer;
pub use surface::{DirectSurface, FrameDescriptor, Surface};
pub use text::{TextEngine, TextRun};
