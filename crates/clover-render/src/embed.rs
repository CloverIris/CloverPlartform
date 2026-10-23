//! Embedded `.cui` documents.
//!
//! A page can place another document inside a box with `<Content Source="…"/>`.
//! Each embedded document owns its own tree and renderer and paints into its own
//! buffer, which is then composited by the parent like any other image.
//!
//! The isolation matters more than the feature: a child that fails to parse, fails
//! to build, or lands on a missing file must leave the parent untouched. The child
//! records its error, the parent keeps rendering, and the failed box is visibly
//! marked instead of silently blank.

use std::sync::Arc;

use clover_core::{Color, Size};
use clover_ui::xaml::UiDocument;
use clover_ui::{Node, UiTree};

use crate::canvas::ImagePixels;
use crate::renderer::CpuRenderer;

/// One embedded document: its tree, its renderer, and its rendered pixels.
pub struct EmbeddedDocument {
    pub source: String,
    pub tree: UiTree,
    pub renderer: CpuRenderer,
    pub texture: Arc<ImagePixels>,
    pub error: Option<String>,
    pub width: u32,
    pub height: u32,
    pub dirty: bool,
}

impl EmbeddedDocument {
    /// Loads and builds `source` at `width` x `height` physical pixels.
    ///
    /// Returns `Err` with a message the caller can surface; the caller keeps the
    /// page alive either way.
    pub fn load(source: &str, width: u32, height: u32, scale: f32) -> Result<Self, String> {
        // A child may be a full `<Ui>` document or a bare fragment; fragments keep
        // small cards readable instead of wrapping every one in boilerplate.
        let document = match UiDocument::from_path(source) {
            Ok(document) => document,
            Err(document_error) => {
                let markup = std::fs::read_to_string(source)
                    .map_err(|error| format!("{source}: {error}"))?;
                UiDocument::parse(&format!(
                    "<Ui>{markup}</Ui>"
                ))
                .map_err(|_| format!("{source}: {document_error}"))?
            }
        };
        let base = document
            .base_dir()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_default();

        let mut renderer = CpuRenderer::new(width.max(1), height.max(1))
            .ok_or_else(|| "no system font for embedded document".to_string())?;
        for (key, relative) in document.resources() {
            let _ = renderer.images.load_png_named(key.clone(), base.join(relative));
        }

        let root = {
            let images = &renderer.images;
            document
                .build(&move |key: &str| images.handle_for(key))
                .map_err(|error| format!("{source}: build failed at {}: {}", error.offset, error.message))?
        };
        let overlay = {
            let images = &renderer.images;
            document.build_overlay(&move |key: &str| images.handle_for(key)).ok().flatten()
        };

        let logical_w = (width as f32 / scale.max(0.01)).max(1.0);
        let logical_h = (height as f32 / scale.max(0.01)).max(1.0);
        let mut tree = UiTree::new(root);
        tree.resize(Size::new(logical_w, logical_h), scale);
        tree.set_overlay(overlay);

        Ok(Self {
            source: source.to_string(),
            tree,
            renderer,
            texture: Arc::new(ImagePixels {
                width: width.max(1),
                height: height.max(1),
                pixels: vec![0; (width.max(1) as usize) * (height.max(1) as usize)],
            }),
            error: None,
            width: width.max(1),
            height: height.max(1),
            dirty: true,
        })
    }

    /// Lays the child out and records any build-time failure.
    pub fn layout(&mut self, width: u32, height: u32, scale: f32) {
        if self.width != width.max(1) || self.height != height.max(1) {
            self.width = width.max(1);
            self.height = height.max(1);
            self.tree.resize(
                Size::new(
                    (self.width as f32 / scale.max(0.01)).max(1.0),
                    (self.height as f32 / scale.max(0.01)).max(1.0),
                ),
                scale,
            );
            self.renderer = match CpuRenderer::new(self.width, self.height) {
                Some(renderer) => renderer,
                None => return,
            };
            self.texture = Arc::new(ImagePixels {
                width: self.width,
                height: self.height,
                pixels: vec![0; (self.width as usize) * (self.height as usize)],
            });
            self.dirty = true;
        }
        // `CpuRenderer` is the layout metrics provider, exactly like the parent.
        let _ = &self.renderer;
        self.tree.layout(&self.renderer);
    }

    /// Renders the child into its texture. Returns true when the texture changed.
    pub fn render(&mut self) -> bool {
        if !self.dirty && !self.tree.has_damage() {
            return false;
        }
        let list = crate::display_list::build(
            &self.tree,
            self.tree.scale(),
            &self.renderer.text,
            &self.renderer.images,
        );
        let bounds = clover_core::Rect::new(0.0, 0.0, list.width as f32, list.height as f32);
        self.renderer.render(&list, &[bounds], clover_core::Premul::TRANSPARENT);
        let canvas = self.renderer.canvas();
        let pixels = canvas.data();
        if let Some(texture) = Arc::get_mut(&mut self.texture) {
            let count = texture.pixels.len().min(pixels.len());
            texture.pixels[..count].copy_from_slice(&pixels[..count]);
        } else {
            self.texture = Arc::new(ImagePixels {
                width: canvas.width,
                height: canvas.height,
                pixels: pixels.to_vec(),
            });
        }
        let _ = self.tree.take_damage(bounds);
        self.dirty = false;
        true
    }

    /// Paints a visible error state into the texture so the host sees which child
    /// failed without reading logs.
    pub fn paint_error(&mut self, message: &str) {
        let color = Color::rgba(0x5A, 0x1E, 0x24, 0xFF).premultiply();
        if let Some(texture) = Arc::get_mut(&mut self.texture) {
            for pixel in texture.pixels.iter_mut() {
                *pixel = color.0;
            }
        }
        self.error = Some(message.to_string());
        self.dirty = false;
    }
}

/// A placeholder tree used when a child cannot even be constructed.
pub fn error_placeholder() -> Node {
    let mut node = clover_ui::node::panel();
    node.style.background = Color::rgb(0x5A, 0x1E, 0x24);
    node
}
