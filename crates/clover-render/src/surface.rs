//! Surfaces: the unit a host window presents.
//!
//! A `Surface` owns a UI tree, a renderer and a damage list, and produces frames
//! on demand. It also owns the registry of [`DirectSurface`] children — the
//! escape hatch that lets another thread write pixels straight into a region of
//! the UI (CloverPic's canvas) without going through the UI thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clover_core::{Premul, Rect, Size};
use clover_ui::{node::Node, NodeId, RouteEntry, SurfaceHandle, UiEvent, UiTree, View};

use crate::canvas::Canvas;
use crate::display_list::build;
use crate::renderer::CpuRenderer;

/// Description of a rendered frame, for handing to a native host.
#[derive(Clone, Copy, Debug)]
pub struct FrameDescriptor {
    /// Premultiplied BGRA8 words.
    pub pixels: *const u32,
    pub width: u32,
    pub height: u32,
    /// Row stride in **pixels** (not bytes).
    pub stride: usize,
    pub premultiplied: bool,
    pub generation: u64,
}

/// A pixel buffer that another thread writes directly.
///
/// The writer and the compositor swap two buffers under a mutex, so publishing a
/// new canvas image never copies the pixels — only the lock is taken. Upgrading
/// this to a lock-free triple buffer is a local change; the API stays the same.
pub struct DirectSurface {
    width: AtomicU32,
    height: AtomicU32,
    front: Mutex<Buffers>,
    version: AtomicU64,
}

struct Buffers {
    front: Vec<u32>,
    back: Vec<u32>,
}

impl DirectSurface {
    pub fn new(width: u32, height: u32) -> Arc<Self> {
        let pixels = vec![0u32; (width.max(1) as usize) * (height.max(1) as usize)];
        Arc::new(Self {
            width: AtomicU32::new(width.max(1)),
            height: AtomicU32::new(height.max(1)),
            front: Mutex::new(Buffers { front: pixels.clone(), back: pixels }),
            version: AtomicU64::new(1),
        })
    }

    pub fn width(&self) -> u32 {
        self.width.load(Ordering::Acquire)
    }

    pub fn height(&self) -> u32 {
        self.height.load(Ordering::Acquire)
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Resizes both buffers, clearing them.
    ///
    /// Takes `&self` so a canvas thread can react to a resize while the UI
    /// thread holds its own handle to the same surface.
    pub fn resize(&self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width() == width && self.height() == height {
            return;
        }
        let len = (width as usize) * (height as usize);
        {
            let mut buffers = self.front.lock().unwrap_or_else(|e| e.into_inner());
            buffers.front = vec![0; len];
            buffers.back = vec![0; len];
        }
        self.width.store(width, Ordering::Release);
        self.height.store(height, Ordering::Release);
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Writes the back buffer and publishes it.
    ///
    /// The closure receives a premultiplied BGRA8 buffer of exactly
    /// `width * height` words. Returning without writing is fine; the previous
    /// contents are preserved only if the closure writes every pixel, so treat
    /// the buffer as undefined.
    pub fn write(&self, f: impl FnOnce(&mut [u32], u32, u32)) {
        {
            let mut guard = self.front.lock().unwrap_or_else(|e| e.into_inner());
            let Buffers { front, back } = &mut *guard;
            f(back, self.width(), self.height());
            std::mem::swap(front, back);
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Removes the back buffer so a native host can write into it directly.
    ///
    /// Pair with [`Self::publish_back`]. Until it is published, the front buffer
    /// keeps showing the previous content, which is the swap-chain guarantee.
    pub fn take_back(&self) -> Vec<u32> {
        let mut guard = self.front.lock().unwrap_or_else(|e| e.into_inner());
        let Buffers { back, .. } = &mut *guard;
        std::mem::take(back)
    }

    /// Publishes a buffer previously removed with [`Self::take_back`].
    pub fn publish_back(&self, buffer: Vec<u32>) {
        {
            let mut guard = self.front.lock().unwrap_or_else(|e| e.into_inner());
            let Buffers { front, back } = &mut *guard;
            let expected = (self.width() as usize) * (self.height() as usize);
            if buffer.len() == expected {
                // Old front becomes the next back buffer: no allocation, no copy.
                *back = std::mem::replace(front, buffer);
            } else {
                *back = vec![0; expected];
            }
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Reads the front buffer.
    pub fn read<R>(&self, f: impl FnOnce(&[u32], u32, u32) -> R) -> R {
        let buffers = self.front.lock().unwrap_or_else(|e| e.into_inner());
        f(&buffers.front, self.width(), self.height())
    }

    /// Raw front-buffer pointer, for native hosts.
    ///
    /// # Safety
    ///
    /// The pointer is only valid until the next [`Self::write`]. Hosts that take
    /// this path are expected to hold the write lock at the platform level.
    pub unsafe fn raw_front(&self) -> (*const u32, usize) {
        let buffers = self.front.lock().unwrap_or_else(|e| e.into_inner());
        (buffers.front.as_ptr(), self.width() as usize)
    }
}

/// A UI surface: tree + renderer + damage.
pub struct Surface {
    tree: UiTree,
    renderer: CpuRenderer,
    direct: HashMap<SurfaceHandle, Arc<DirectSurface>>,
    generation: u64,
    /// Actions raised since the host last drained the queue.
    pending_actions: Vec<String>,
    /// Node that will fire if the pointer is released over it.
    press_action: Option<NodeId>,
    /// Active drag gesture, if any.
    drag: Option<DragState>,
    /// Widget currently being dragged (slider thumb, scrollbar thumb).
    widget_drag: Option<NodeId>,
    /// Widget pressed and waiting for release to act (checkbox, combo).
    widget_press: Option<NodeId>,
    /// Open combo box: the node and its name, so the popup can be rebuilt.
    combo: Option<(NodeId, String)>,
    /// Active splitter drag: (splitter node, size at press, pointer at press).
    splitter: Option<(NodeId, f32, f32)>,
    /// Embedded documents, keyed by their resolved path.
    embeds: HashMap<String, crate::embed::EmbeddedDocument>,
    /// Rows of the open popup, in display order.
    menu_rows: Vec<(NodeId, usize)>,
    /// Highlighted row index inside the open popup.
    menu_highlight: usize,
}

impl Surface {
    /// Creates a surface with a system font.
    pub fn new(root: Node, width: f32, height: f32, scale: f32) -> Option<Self> {
        let physical = physical_size(width, height, scale);
        let renderer = CpuRenderer::new(physical.0, physical.1)?;
        let mut surface = Self {
            tree: UiTree::new(root),
            renderer,
            direct: HashMap::new(),
            generation: 0,
            pending_actions: Vec::new(),
            press_action: None,
            drag: None,
            widget_drag: None,
            widget_press: None,
            combo: None,
            splitter: None,
            embeds: HashMap::new(),
            menu_rows: Vec::new(),
            menu_highlight: 0,
        };
        surface.resize(width, height, scale);
        Some(surface)
    }

    pub fn tree(&self) -> &UiTree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut UiTree {
        &mut self.tree
    }

    pub fn renderer(&self) -> &CpuRenderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut CpuRenderer {
        &mut self.renderer
    }

    pub fn canvas(&self) -> &Canvas {
        self.renderer.canvas()
    }

    pub fn resize(&mut self, width: f32, height: f32, scale: f32) {
        let size = Size::new(width, height);
        self.tree.resize(size, scale);
        let (w, h) = physical_size(width, height, scale);
        self.renderer.resize(w, h);
        self.tree.damage().mark_full();
    }

    /// Replaces the UI tree and schedules a full repaint.
    pub fn set_root(&mut self, root: Node) {
        self.tree.set_root(root);
        self.pending_actions.clear();
        self.press_action = None;
    }

    /// Registers a direct-memory child surface.
    pub fn register_direct(&mut self, handle: SurfaceHandle, surface: Arc<DirectSurface>) {
        self.direct.insert(handle, surface);
        self.tree.damage().mark_full();
    }

    pub fn direct(&self, handle: SurfaceHandle) -> Option<&Arc<DirectSurface>> {
        self.direct.get(&handle)
    }

    /// Marks the whole surface dirty.
    pub fn invalidate_all(&mut self) {
        self.tree.damage().mark_full();
    }

    /// Returns the current frame descriptor, rendering first if anything is dirty.
    pub fn frame(&mut self) -> FrameDescriptor {
        self.render_if_needed();
        let canvas = self.renderer.canvas();
        FrameDescriptor {
            pixels: canvas.data().as_ptr(),
            width: canvas.width,
            height: canvas.height,
            stride: canvas.stride(),
            premultiplied: true,
            generation: self.generation,
        }
    }

    /// Renders dirty regions, if any.
    pub fn render_if_needed(&mut self) {
        self.tree.flush();
        if !self.tree.has_damage() {
            return;
        }
        let metrics = &self.renderer;
        self.tree.layout(metrics);
        // Children must be laid out and rendered before the parent's display list
        // is built, because the parent only sees their textures.
        self.sync_embeds();

        let list = build(&self.tree, self.tree.scale(), &self.renderer.text, &self.renderer.images);
        // The tree works in logical pixels; the renderer works in physical ones.
        // Damage must be converted or hovers repaint the wrong region at >1x.
        let viewport = self.tree.viewport();
        let logical_bounds = Rect::new(0.0, 0.0, viewport.w, viewport.h);
        let scale = self.tree.scale();
        let damage: Vec<Rect> = self
            .tree
            .take_damage(logical_bounds)
            .into_iter()
            .map(|rect| rect.scale(scale))
            .collect();
        if damage.is_empty() {
            return;
        }
        self.renderer.render(&list, &damage, Premul::TRANSPARENT);
        self.composite_direct_surfaces(&list);
        self.generation += 1;
    }

    /// Range and source of the embedded document under `point`, if any.
    fn content_at(&self, point: clover_core::Point) -> Option<(String, Rect)> {
        fn walk(node: &Node, point: clover_core::Point, out: &mut Option<(String, Rect)>) {
            if out.is_some() || !node.visible {
                return;
            }
            if let View::Content { source } = &node.view {
                if node.bounds.contains(point) {
                    *out = Some((source.clone(), node.bounds));
                    return;
                }
            }
            for child in &node.children {
                walk(child, point, out);
            }
        }
        let mut found = None;
        walk(self.tree.root(), point, &mut found);
        found
    }

    /// The embedded document for a slot node named `slot`.
    fn embed_for_slot(&mut self, slot: &str) -> Option<&mut crate::embed::EmbeddedDocument> {
        let node = self.tree.named(slot)?;
        let source = match &node.view {
            View::Content { source } => source.clone(),
            _ => return None,
        };
        self.embeds.get_mut(&source)
    }

    /// Marks the box of a slot for repaint after its document changed.
    fn damage_slot(&mut self, slot: &str) {
        if let Some(node) = self.tree.named(slot) {
            let bounds = node.bounds;
            self.tree.damage().add(bounds);
        }
    }

    /// Splits `slot:node` into its parts; a plain name addresses the page.
    fn split_slot(name: &str) -> Option<(String, String)> {
        name.split_once(':')
            .map(|(slot, node)| (slot.to_string(), node.to_string()))
    }

    /// Loads, lays out and renders every embedded document.
    ///
    /// Failures are contained: a child that cannot load is recorded, marked on
    /// screen, and skipped. The parent keeps rendering.
    fn sync_embeds(&mut self) {
        let scale = self.tree.scale();
        let mut wanted: Vec<(String, u32, u32)> = Vec::new();
        collect_content(self.tree.root(), scale, &mut wanted);
        if let Some(overlay) = self.tree.overlay() {
            collect_content(overlay, scale, &mut wanted);
        }
        if wanted.is_empty() {
            return;
        }

        for (source, width, height) in wanted {
            let key = format!("__content:{source}");
            let entry = self.embeds.remove(&source);
            let mut embed = match entry {
                Some(embed) => embed,
                None => match crate::embed::EmbeddedDocument::load(&source, width, height, scale) {
                    Ok(embed) => embed,
                    Err(error) => {
                        std::eprintln!("[clover] embedded document failed: {error}");
                        let mut placeholder = crate::embed::error_placeholder();
                        placeholder.style.width = clover_ui::Length::Fill;
                        placeholder.style.height = clover_ui::Length::Fill;
                        let mut pixels = crate::canvas::ImagePixels {
                            width: width.max(1),
                            height: height.max(1),
                            pixels: vec![0; (width.max(1) as usize) * (height.max(1) as usize)],
                        };
                        let tint = clover_core::Color::rgb(0x5A, 0x1E, 0x24).premultiply();
                        for pixel in pixels.pixels.iter_mut() {
                            *pixel = tint.0;
                        }
                        self.renderer.images.insert_named(key, pixels);
                        continue;
                    }
                },
            };
            embed.layout(width, height, scale);
            let changed = embed.render();
            if changed {
                let texture = crate::canvas::ImagePixels {
                    width: embed.texture.width,
                    height: embed.texture.height,
                    pixels: embed.texture.pixels.clone(),
                };
                self.renderer.images.insert_named(key, texture);
            }
            self.embeds.insert(source, embed);
        }
    }

    /// Replaces what an embedded document shows, without touching its box.
    pub fn set_content(&mut self, name: &str, source: &str) -> bool {
        let Some(node) = self.tree.named_mut(name) else { return false };
        let View::Content { source: current } = &mut node.view else { return false };
        if *current == source {
            return false;
        }
        self.embeds.remove(current);
        let _ = self.renderer.images.handle_for(&format!("__content:{source}"));
        *current = source.to_string();
        self.tree.invalidate_layout();
        self.tree.damage().mark_full();
        true
    }

    /// Errors reported by embedded documents, for diagnostics.
    pub fn embed_errors(&self) -> Vec<(String, String)> {
        self.embeds
            .values()
            .filter_map(|embed| embed.error.clone().map(|error| (embed.source.clone(), error)))
            .collect()
    }

    fn composite_direct_surfaces(&mut self, list: &crate::display_list::DisplayList) {
        for (handle, rect, clip) in list.surfaces() {
            let Some(surface) = self.direct.get(&handle).cloned() else { continue };
            let canvas = self.renderer.canvas_mut();
            surface.read(|pixels, width, height| {
                if width == 0 || height == 0 {
                    return;
                }
                let dest = rect;
                let sx = width as f32 / dest.w.max(1.0);
                let sy = height as f32 / dest.h.max(1.0);
                let area = dest.intersect(clip);
                if area.is_empty() {
                    return;
                }
                for y in area.y.floor() as i32..area.bottom().ceil() as i32 {
                    for x in area.x.floor() as i32..area.right().ceil() as i32 {
                        let u = ((x as f32 + 0.5 - dest.x) * sx).clamp(0.0, width as f32 - 0.001);
                        let v = ((y as f32 + 0.5 - dest.y) * sy).clamp(0.0, height as f32 - 0.001);
                        let index = (v as usize) * (width as usize) + u as usize;
                        let color = Premul(*pixels.get(index).unwrap_or(&0));
                        if color.a() == 0 {
                            continue;
                        }
                        canvas.blend(x, y, color, clip);
                    }
                }
            });
        }
    }

    /// Nearest actionable ancestor of the deepest hit node.
    fn action_target(&self, chain: &[NodeId]) -> Option<(NodeId, String)> {
        chain.iter().find_map(|id| {
            self.tree
                .node(*id)
                .and_then(|node| node.action.as_ref().map(|action| (node.id, action.clone())))
        })
    }

    /// Applies one keystroke to the focused text field.
    ///
    /// Editing keys are handled here rather than by the host because the caret
    /// and the buffer belong to the tree; the host only learns that the text
    /// changed, and reads it back when it cares.
    fn edit_text_input(&mut self, focused: NodeId, key: u32, modifiers: clover_ui::Modifiers) {
        let mut changed = false;
        let mut submit = false;
        let mut bounds = Rect::ZERO;
        let mut name: Option<String> = None;

        if let Some(node) = self.tree.node_mut(focused) {
            bounds = node.bounds;
            name = node.name.clone();
            if let View::TextInput { text, caret, .. } = &mut node.view {
                let mut position = (*caret).min(text.len());
                match key {
                    0x08 => {
                        if position > 0 {
                            let previous = previous_boundary(text, position);
                            text.replace_range(previous..position, "");
                            position = previous;
                            changed = true;
                        }
                    }
                    0x2E => {
                        if position < text.len() {
                            let next = next_boundary(text, position);
                            text.replace_range(position..next, "");
                            changed = true;
                        }
                    }
                    0x25 => position = previous_boundary(text, position),
                    0x27 => position = next_boundary(text, position),
                    0x24 => position = 0,
                    0x23 => position = text.len(),
                    0x0D => submit = true,
                    // Printable ASCII; letters arrive as virtual-key upper case.
                    _ if (0x20..=0x7E).contains(&key) => {
                        if let Some(ch) = char::from_u32(key) {
                            let ch = if ch.is_ascii_uppercase() && !modifiers.shift() {
                                ch.to_ascii_lowercase()
                            } else {
                                ch
                            };
                            text.insert(position, ch);
                            position += ch.len_utf8();
                            changed = true;
                        }
                    }
                    _ => {}
                }
                *caret = position;
            }
        }

        if changed || submit {
            self.tree.damage().add(bounds);
        }
        if let Some(name) = name {
            if submit {
                self.pending_actions.push(format!("submit:{name}"));
            } else if changed {
                self.pending_actions.push(format!("text:{name}"));
            }
        }
    }

    /// Current contents of a named text field.
    pub fn text_of(&self, name: &str) -> Option<String> {
        match self.tree.named(name).map(|node| &node.view) {
            Some(View::TextInput { text, .. }) => Some(text.clone()),
            _ => None,
        }
    }

    /// Replaces the contents of a named text field and moves the caret to the end.
    pub fn set_text(&mut self, name: &str, value: impl Into<String>) -> bool {
        let value = value.into();
        let mut bounds = Rect::ZERO;
        let mut found = false;
        if let Some(node) = self.tree.named_mut(name) {
            if let View::TextInput { text, caret, .. } = &mut node.view {
                *text = value;
                *caret = text.len();
                bounds = node.bounds;
                found = true;
            }
        }
        if found {
            self.tree.damage().add(bounds);
        }
        found
    }

    /// Sets the input-method composition on the focused text field.
    ///
    /// The composition is preview text: it renders inline with an underline and
    /// only becomes part of the field on [`Self::ime_commit`].
    pub fn ime_mark(&mut self, text: impl Into<String>) -> bool {
        let text = text.into();
        let mut bounds = Rect::ZERO;
        let mut changed = false;
        if let Some(node) = self.focused_input_mut() {
            if let View::TextInput { composition, .. } = &mut node.view {
                let cursor = text.len();
                *composition = Some(clover_ui::node::Composition { text, cursor });
                bounds = node.bounds;
                changed = true;
            }
        }
        if changed {
            self.tree.damage().add(bounds);
        }
        changed
    }

    /// Clears the composition without inserting anything.
    pub fn ime_cancel(&mut self) -> bool {
        let mut bounds = Rect::ZERO;
        let mut changed = false;
        if let Some(node) = self.focused_input_mut() {
            if let View::TextInput { composition, .. } = &mut node.view {
                if composition.take().is_some() {
                    bounds = node.bounds;
                    changed = true;
                }
            }
        }
        if changed {
            self.tree.damage().add(bounds);
        }
        changed
    }

    /// Commits the composition into the field.
    pub fn ime_commit(&mut self) -> bool {
        let Some(focused) = self.tree.focus() else { return false };
        let mut committed = None;
        if let Some(node) = self.tree.node_mut(focused) {
            if let View::TextInput { text, caret, composition, .. } = &mut node.view {
                if let Some(composition) = composition.take() {
                    let position = (*caret).min(text.len());
                    text.insert_str(position, &composition.text);
                    *caret = position + composition.text.len();
                    committed = Some(node.name.clone());
                }
            }
        }
        let Some(name) = committed else { return false };
        let bounds = self.tree.node(focused).map(|node| node.bounds).unwrap_or(Rect::ZERO);
        self.tree.damage().add(bounds);
        if let Some(name) = name {
            self.pending_actions.push(format!("text:{name}"));
        }
        true
    }

    /// Inserts committed text at the caret, clearing any composition.
    ///
    /// This is the path a platform's `insertText:` takes: the IME has already
    /// resolved the characters, so no composition state is involved.
    pub fn ime_insert(&mut self, value: impl AsRef<str>) -> bool {
        let value = value.as_ref();
        if value.is_empty() {
            return false;
        }
        let Some(focused) = self.tree.focus() else { return false };
        let mut name = None;
        let mut bounds = Rect::ZERO;
        let mut inserted = false;
        if let Some(node) = self.tree.node_mut(focused) {
            if let View::TextInput { text, caret, composition, .. } = &mut node.view {
                composition.take();
                let position = (*caret).min(text.len());
                text.insert_str(position, value);
                *caret = position + value.len();
                name = node.name.clone();
                bounds = node.bounds;
                inserted = true;
            }
        }
        if !inserted {
            return false;
        }
        self.tree.damage().add(bounds);
        if let Some(name) = name {
            self.pending_actions.push(format!("text:{name}"));
        }
        true
    }

    /// Caret rectangle of the focused text field, in logical pixels.
    ///
    /// Hosts use this to place the IME candidate window.
    pub fn focused_caret_rect(&self) -> Option<Rect> {
        let focused = self.tree.focus()?;
        let node = self.tree.node(focused)?;
        let View::TextInput { text, caret, composition, .. } = &node.view else {
            return None;
        };
        let (displayed, range) = clover_ui::node::text_input_display(text, composition.as_ref(), *caret);
        let caret_byte = range.map(|(_, end)| end).unwrap_or((*caret).min(displayed.len()));
        let prefix = &displayed[..caret_byte.min(displayed.len())];
        let font_px = node.style.font_size.max(1.0);
        let line_height = self.renderer.text.line_height(font_px);
        let content = node.style.padding.deflate(node.bounds);
        let x = content.x + self.renderer.text.line_width(prefix, font_px);
        let y = content.y + ((content.h - line_height) * 0.5).max(0.0);
        Some(Rect::new(x, y, 1.0, line_height))
    }

    fn focused_input_mut(&mut self) -> Option<&mut Node> {
        let focused = self.tree.focus()?;
        let node = self.tree.node_mut(focused)?;
        match node.view {
            View::TextInput { .. } => Some(node),
            _ => None,
        }
    }

    /// Starts animating a named node's opacity towards `target`.
    pub fn animate_opacity(&mut self, name: &str, target: f32, duration_ms: u64, now_ms: u64) -> bool {
        self.start_animation(name, StyleTarget::Opacity(target.clamp(0.0, 1.0)), duration_ms, now_ms)
    }

    /// Starts animating a named node's background colour towards `target`.
    pub fn animate_background(&mut self, name: &str, target: clover_core::Color, duration_ms: u64, now_ms: u64) -> bool {
        self.start_animation(name, StyleTarget::Background(target), duration_ms, now_ms)
    }

    fn start_animation(&mut self, name: &str, target: StyleTarget, duration_ms: u64, now_ms: u64) -> bool {
        let Some(node) = self.tree.named_mut(name) else { return false };
        let mut animation = clover_ui::node::Animation {
            start_ms: now_ms,
            duration_ms,
            from_opacity: node.style.opacity,
            to_opacity: node.style.opacity,
            from_background: node.style.background,
            to_background: node.style.background,
        };
        match target {
            StyleTarget::Opacity(value) => animation.to_opacity = value,
            StyleTarget::Background(color) => animation.to_background = color,
        }
        node.animation = Some(animation);
        true
    }

    /// Advances every running animation to `now_ms`.
    ///
    /// Returns `true` while any animation is still running, so a host can keep
    /// requesting frames and then stop.
    pub fn advance(&mut self, now_ms: u64) -> bool {
        let mut changed: Vec<Rect> = Vec::new();
        let mut active = false;
        advance_node(self.tree.root_mut(), now_ms, &mut changed, &mut active);
        for rect in changed {
            self.tree.damage().add(rect);
        }
        active
    }

    /// Replaces the overlay root; popups live here so nothing clips them.
    pub fn set_overlay(&mut self, overlay: Option<Node>) -> bool {
        self.tree.set_overlay(overlay);
        true
    }

    /// Points a named `<CloverSurface>` node at a live direct region.
    ///
    /// Documents cannot know the handle a region will get, so the host binds it
    /// after creating the region instead of hard-coding `Id` in markup.
    pub fn bind_direct_surface(&mut self, name: &str, handle: SurfaceHandle) -> bool {
        let Some(node) = self.tree.named_mut(name) else { return false };
        if let View::Surface { handle: current } = &mut node.view {
            *current = handle;
            let bounds = node.bounds;
            self.tree.damage().add(bounds);
            self.tree.invalidate_layout();
            return true;
        }
        false
    }

    /// Pushes a value into a slider or scrollbar (the MVVM update direction).
    pub fn set_value(&mut self, name: &str, value: f32) -> bool {
        // `slot:node` addresses a control inside an embedded document, so panels
        // can own their controls without colliding with the page.
        if let Some((slot, node)) = Self::split_slot(name) {
            let changed = match self.embed_for_slot(&slot) {
                Some(embed) => {
                    let result = embed.tree.set_value(&node, value);
                    if result {
                        embed.dirty = true;
                    }
                    result
                }
                None => false,
            };
            if changed {
                self.damage_slot(&slot);
            }
            return changed;
        }
        self.tree.set_value(name, value)
    }

    /// Pushes a checked state into a checkbox.
    pub fn set_checked(&mut self, name: &str, checked: bool) -> bool {
        // `slot:node` addresses a control inside an embedded document, so panels
        // can own their controls without colliding with the page.
        if let Some((slot, node)) = Self::split_slot(name) {
            let changed = match self.embed_for_slot(&slot) {
                Some(embed) => {
                    let result = embed.tree.set_checked(&node, checked);
                    if result {
                        embed.dirty = true;
                    }
                    result
                }
                None => false,
            };
            if changed {
                self.damage_slot(&slot);
            }
            return changed;
        }
        self.tree.set_checked(name, checked)
    }

    /// Replaces a combo box's items, e.g. when the host supplies the real list.
    ///
    /// Rebuilds the open popup so a list that changes while shown cannot go stale.
    pub fn set_items(&mut self, name: &str, items: Vec<String>) -> bool {
        let Some(id) = self.tree.named(name).map(|node| node.id) else { return false };
        let Some(node) = self.tree.node_mut(id) else { return false };
        if let Some(clover_ui::node::Widget::ComboBox { items: current, selected, .. }) =
            &mut node.widget
        {
            if *current == items {
                return false;
            }
            *current = items;
            *selected = (*selected).min(current.len().saturating_sub(1));
            let bounds = node.bounds;
            self.tree.damage().add(bounds);
            if self.combo.as_ref().map(|(open, _)| *open == id).unwrap_or(false) {
                self.refresh_popup();
            }
            return true;
        }
        false
    }

    /// Pushes a selection into a combo box.
    pub fn set_selected(&mut self, name: &str, index: usize) -> bool {
        // `slot:node` addresses a control inside an embedded document, so panels
        // can own their controls without colliding with the page.
        if let Some((slot, node)) = Self::split_slot(name) {
            let changed = match self.embed_for_slot(&slot) {
                Some(embed) => {
                    let result = embed.tree.set_selected(&node, index);
                    if result {
                        embed.dirty = true;
                    }
                    result
                }
                None => false,
            };
            if changed {
                self.damage_slot(&slot);
            }
            return changed;
        }
        self.tree.set_selected(name, index)
    }

    /// Enables or disables a node, dimming it when disabled.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        // `slot:node` addresses a control inside an embedded document, so panels
        // can own their controls without colliding with the page.
        if let Some((slot, node)) = Self::split_slot(name) {
            let changed = match self.embed_for_slot(&slot) {
                Some(embed) => {
                    let result = embed.tree.set_enabled(&node, enabled);
                    if result {
                        embed.dirty = true;
                    }
                    result
                }
                None => false,
            };
            if changed {
                self.damage_slot(&slot);
            }
            return changed;
        }
        self.tree.set_enabled(name, enabled)
    }

    /// Replaces a named node's children, for dynamic lists.
    pub fn set_children(&mut self, name: &str, children: Vec<Node>) -> bool {
        let Some(node) = self.tree.named_mut(name) else { return false };
        node.children = children;
        self.tree.invalidate_layout();
        self.tree.damage().mark_full();
        true
    }

    /// Rebuilds the whole page from a `.cui` document, resources included.
    pub fn reload_cui(&mut self, path: &str) -> bool {
        let Ok(document) = clover_ui::xaml::UiDocument::from_path(path) else {
            return false;
        };
        let base = std::path::Path::new(path)
            .parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_default();
        for (key, relative) in document.resources() {
            let _ = self
                .renderer
                .images
                .load_png_named(key.clone(), base.join(relative));
        }
        let root = {
            let images = &self.renderer.images;
            match document.build(&move |source: &str| images.handle_for(source)) {
                Ok(root) => root,
                Err(_) => return false,
            }
        };
        let overlay = {
            let images = &self.renderer.images;
            match document.build_overlay(&move |source: &str| images.handle_for(source)) {
                Ok(overlay) => overlay,
                Err(_) => return false,
            }
        };
        self.tree.set_overlay(overlay);
        self.tree.set_root(root);
        true
    }

    /// Updates a text node's content, e.g. a status readout.
    pub fn set_label(&mut self, name: &str, value: &str) -> bool {
        // `slot:node` addresses a control inside an embedded document, so panels
        // can own their controls without colliding with the page.
        if let Some((slot, node)) = Self::split_slot(name) {
            let changed = match self.embed_for_slot(&slot) {
                Some(embed) => {
                    let result = embed.tree.set_label(&node, value);
                    if result {
                        embed.dirty = true;
                    }
                    result
                }
                None => false,
            };
            if changed {
                self.damage_slot(&slot);
            }
            return changed;
        }
        self.tree.set_label(name, value)
    }

    /// Cursor the host should show at a point, in logical pixels.
    pub fn cursor_at(&self, x: f32, y: f32) -> clover_ui::style::Cursor {
        self.tree.cursor_at(clover_core::Point::new(x, y))
    }

    /// Handles a press on a widget. Returns true when the widget consumed it.
    fn begin_widget(&mut self, id: NodeId, position: clover_core::Point) -> bool {
        let kind = match self.tree.node(id).and_then(|node| node.widget.as_ref()) {
            Some(widget) => widget.clone(),
            None => return false,
        };
        match kind {
            clover_ui::node::Widget::Slider { .. } | clover_ui::node::Widget::ScrollBar { .. } => {
                self.widget_drag = Some(id);
                self.drag_widget_to(id, position);
                true
            }
            clover_ui::node::Widget::Splitter { horizontal, .. } => {
                let (start, position_along) = if horizontal {
                    (self.pane_size(id), position.x)
                } else {
                    (self.pane_size(id), position.y)
                };
                self.splitter = Some((id, start, position_along));
                true
            }
            clover_ui::node::Widget::CheckBox { .. } => {
                self.widget_press = Some(id);
                true
            }
            clover_ui::node::Widget::ComboBox { .. } => {
                self.widget_press = Some(id);
                true
            }
        }
    }

    /// Moves a slider or scrollbar to the pointer, without emitting an action yet.
    fn drag_widget_to(&mut self, id: NodeId, position: clover_core::Point) {
        let Some(node) = self.tree.node_mut(id) else { return };
        let bounds = node.bounds;
        match &mut node.widget {
            Some(clover_ui::node::Widget::Slider { value, min, max, show_value }) => {
                let label = if *show_value { 34.0 } else { 0.0 };
                let width = (bounds.w - label).max(1.0);
                let t = ((position.x - bounds.x) / width).clamp(0.0, 1.0);
                *value = *min + (*max - *min) * t;
                self.tree.damage().add(bounds);
            }
            Some(clover_ui::node::Widget::ScrollBar { horizontal, offset, viewport, extent, .. }) => {
                let span = (*extent - *viewport).max(0.0);
                let (length, start, position_along) = if *horizontal {
                    (bounds.w, bounds.x, position.x)
                } else {
                    (bounds.h, bounds.y, position.y)
                };
                if length <= 0.0 || span <= 0.0 {
                    return;
                }
                let t = ((position_along - start) / length).clamp(0.0, 1.0);
                *offset = (t * *extent - *viewport * 0.5).clamp(0.0, span);
                self.tree.damage().add(bounds);
            }
            _ => {}
        }
    }

    /// Emits the `change:` action for a widget that was dragged, and applies the
    /// widget's own effect first (a scrollbar moves the container it names).
    fn emit_widget_change(&mut self, id: NodeId) {
        let mut target = None;
        let mut offset = 0.0;
        if let Some(node) = self.tree.node(id) {
            if let Some(clover_ui::node::Widget::ScrollBar { target: name, offset: value, .. }) =
                node.widget.as_ref()
            {
                target = name.clone();
                offset = *value;
            }
        }
        if let Some(name) = target {
            if let Some(container) = self.tree.named_mut(&name) {
                container.scroll.offset = offset;
                let bounds = container.bounds;
                self.tree.damage().add(bounds);
                // The offset is applied while arranging children, so the layout
                // pass has to run again or the content never moves.
                self.tree.invalidate_layout();
            }
        }
        if let Some(action) = self.tree.node(id).and_then(|node| node.widget_action()) {
            self.pending_actions.push(action);
        }
    }

    /// Applies a click on a widget that acts on release.
    fn activate_widget(&mut self, id: NodeId) {
        let Some(node) = self.tree.node_mut(id) else { return };
        let bounds = node.bounds;
        match &mut node.widget {
            Some(clover_ui::node::Widget::CheckBox { checked }) => {
                *checked = !*checked;
            }
            Some(clover_ui::node::Widget::ComboBox { open, .. }) => {
                *open = !*open;
            }
            _ => {}
        }
        self.tree.damage().add(bounds);

        let mut open = false;
        let mut is_combo = false;
        if let Some(clover_ui::node::Widget::ComboBox { open: toggled, .. }) =
            self.tree.node(id).and_then(|node| node.widget.as_ref())
        {
            is_combo = true;
            open = *toggled;
        }
        if is_combo {
            // Opening or dismissing the list is not a value change; only choosing
            // an item is, and that path goes through `handle_internal`.
            if let Some(name) = self.tree.node(id).and_then(|node| node.name.clone()) {
                if open {
                    self.open_combo(id, &name);
                } else {
                    self.combo = None;
                    self.tree.set_overlay(None);
                }
            }
            return;
        }
        self.emit_widget_change(id);
    }

    /// Whether the hit chain reaches into the popup currently on screen.
    fn hit_is_inside_popup(&self, chain: &[NodeId]) -> bool {
        let Some(overlay) = self.tree.overlay() else { return false };
        let overlay_id = overlay.id;
        chain.iter().any(|id| *id == overlay_id)
    }

    /// Size of the pane a splitter drives, in its drag axis.
    fn pane_size(&self, splitter: NodeId) -> f32 {
        let Some(node) = self.tree.node(splitter) else { return 0.0 };
        let Some(clover_ui::node::Widget::Splitter { horizontal, target, .. }) = node.widget.as_ref()
        else {
            return 0.0;
        };
        let Some(name) = target.as_deref() else { return 0.0 };
        let Some(pane) = self.tree.named(name) else { return 0.0 };
        if *horizontal {
            pane.bounds.w
        } else {
            pane.bounds.h
        }
    }

    /// Resizes the pane a splitter drives.
    fn drag_splitter(&mut self, splitter: NodeId, start: f32, origin: f32, position: clover_core::Point) {
        let Some(node) = self.tree.node(splitter) else { return };
        let Some(clover_ui::node::Widget::Splitter { horizontal, target, min, max }) =
            node.widget.as_ref()
        else {
            return;
        };
        let (horizontal, min, max) = (*horizontal, *min, *max);
        let Some(name) = target.clone() else { return };
        let delta = if horizontal { position.x - origin } else { position.y - origin };
        let size = (start + delta).clamp(min, max);
        if let Some(pane) = self.tree.named_mut(&name) {
            if horizontal {
                pane.style.width = clover_ui::style::Length::Px(size);
            } else {
                pane.style.height = clover_ui::style::Length::Px(size);
            }
        }
        self.tree.invalidate_layout();
        self.tree.damage().mark_full();
    }

    /// Rebuilds the open popup with the current highlight.
    fn refresh_popup(&mut self) {
        let Some((id, name)) = self.combo.clone() else { return };
        self.open_combo_again(id, &name);
    }

    /// Re-opens a combo's popup without resetting the highlight.
    fn open_combo_again(&mut self, id: NodeId, name: &str) {
        let Some(node) = self.tree.node(id) else { return };
        let anchor = node.bounds;
        let items = match node.widget.as_ref() {
            Some(clover_ui::node::Widget::ComboBox { items, .. }) => items.clone(),
            _ => return,
        };
        let selected = self.menu_highlight;
        self.build_popup(anchor, &items, selected, name);
    }

    /// Closes the combo popup and marks the box closed.
    fn close_combo(&mut self) {
        if let Some((id, _)) = self.combo.take() {
            if let Some(node) = self.tree.node_mut(id) {
                if let Some(clover_ui::node::Widget::ComboBox { open, .. }) = &mut node.widget {
                    *open = false;
                }
            }
        }
        self.tree.set_overlay(None);
    }

    /// Handles a framework-internal action such as `@combo:blend:2`.
    fn handle_internal(&mut self, action: &str) {
        let rest = action.trim_start_matches('@');
        let mut parts = rest.split(':');
        let kind = parts.next().unwrap_or_default();
        if kind != "combo" && kind != "menu" && kind != "submenu" {
            return;
        }
        let name = parts.next().unwrap_or_default();
        let index: usize = parts.next().and_then(|value| value.parse().ok()).unwrap_or(0);

        if kind == "submenu" {
            // Keep the parent menu open; the host decides what the submenu holds.
            self.pending_actions.push(format!("{name}:{index}"));
            return;
        }

        if kind == "menu" {
            // A context menu has no widget behind it: close and tell the host.
            self.combo = None;
            self.menu_rows.clear();
            self.menu_highlight = 0;
            self.tree.set_overlay(None);
            self.pending_actions.push(format!("{name}:{index}"));
            return;
        }

        let mut target = None;
        if let Some(node) = self.tree.named(name) {
            target = Some(node.id);
        }
        let Some(id) = target else { return };
        let items = match self.tree.node(id).and_then(|node| node.widget.as_ref()) {
            Some(clover_ui::node::Widget::ComboBox { items, .. }) => items.clone(),
            _ => return,
        };
        if let Some(node) = self.tree.node_mut(id) {
            if let Some(clover_ui::node::Widget::ComboBox { selected, open, .. }) = &mut node.widget {
                *selected = index.min(items.len().saturating_sub(1));
                *open = false;
            }
            let bounds = node.bounds;
            self.tree.damage().add(bounds);
        }
        self.combo = None;
        self.menu_rows.clear();
        self.menu_highlight = 0;
        self.tree.set_overlay(None);
        self.pending_actions.push(format!("change:{name}:{index}"));
    }

    /// Builds and shows the popup list for a combo box.
    ///
    /// Placement is decided against the viewport, not against the anchor: a list
    /// opened near the bottom flips up instead of running off screen, and a list
    /// taller than the window scrolls.
    fn open_combo(&mut self, id: NodeId, name: &str) {
        let Some(node) = self.tree.node(id) else { return };
        let anchor = node.bounds;
        let (items, selected) = match node.widget.as_ref() {
            Some(clover_ui::node::Widget::ComboBox { items, selected, .. }) => {
                (items.clone(), *selected)
            }
            _ => return,
        };
        self.menu_highlight = selected.min(items.len().saturating_sub(1));
        self.build_popup(anchor, &items, selected, name);
        self.combo = Some((id, name.to_string()));
        if let Some(node) = self.tree.node_mut(id) {
            if let Some(clover_ui::node::Widget::ComboBox { open, .. }) = &mut node.widget {
                *open = true;
            }
        }
    }

    /// Opens a menu at `anchor` whose rows are reported as `<prefix>:<index>`.
    ///
    /// This is the same popup as a combo box, so clamping, scrolling, hover and
    /// keyboard behaviour are identical. Context menus, submenus and palette
    /// flyouts all go through here.
    pub fn open_menu(&mut self, anchor: Rect, items: Vec<String>, prefix: &str) -> bool {
        if items.is_empty() {
            return false;
        }
        self.combo = None;
        self.menu_highlight = usize::MAX;
        self.build_popup_with(anchor, &items, usize::MAX, prefix, "@menu");
        true
    }

    /// Opens a submenu to the right of the popup that is currently open.
    ///
    /// Rows behave like any other menu, and choosing one closes the whole cascade.
    pub fn open_submenu(&mut self, items: Vec<String>, prefix: &str) -> bool {
        if items.is_empty() {
            return false;
        }
        let parent = match self.tree.overlay() {
            Some(overlay) => overlay.children.first().map(|list| list.bounds),
            None => None,
        };
        let anchor = parent
            .map(|bounds| {
                clover_core::Rect::new(bounds.right() - 4.0, bounds.y + 4.0, bounds.w, 1.0)
            })
            .unwrap_or(clover_core::Rect::new(0.0, 0.0, 160.0, 1.0));
        self.menu_highlight = usize::MAX;
        self.build_popup_with(anchor, &items, usize::MAX, prefix, "@menu");
        true
    }

    /// Closes any open popup.
    pub fn close_popup(&mut self) -> bool {
        let had = self.tree.overlay().is_some();
        self.combo = None;
        self.menu_rows.clear();
        self.menu_highlight = 0;
        self.tree.set_overlay(None);
        had
    }

    /// Lays out a popup list under (or above) `anchor`.
    ///
    /// Shared by combo boxes and, later, context menus: both need the same
    /// clamping, hover, scrolling and keyboard behaviour.
    fn build_popup(&mut self, anchor: Rect, items: &[String], selected: usize, name: &str) {
        self.build_popup_with(anchor, items, selected, name, "@combo");
    }

    /// Builds a popup whose rows emit `<@combo|@menu>:<name>:<index>`.
    fn build_popup_with(
        &mut self,
        anchor: Rect,
        items: &[String],
        selected: usize,
        name: &str,
        kind: &str,
    ) {
        const ROW_HEIGHT: f32 = 22.0;
        const PADDING: f32 = 3.0;
        const MAX_ROWS_VISIBLE: f32 = 12.0;

        let viewport = self.tree.viewport();
        let rows = items.len() as f32;
        let wanted = rows * ROW_HEIGHT + PADDING * 2.0;
        let max_height = (MAX_ROWS_VISIBLE * ROW_HEIGHT + PADDING * 2.0).min(viewport.h - 16.0);
        let height = wanted.min(max_height.max(ROW_HEIGHT + PADDING * 2.0));

        let below = anchor.y + anchor.h + 2.0;
        let fits_below = below + height <= viewport.h - 4.0;
        let y = if fits_below {
            below
        } else {
            // Flip above; if neither side fits, keep the larger share visible.
            (anchor.y - height - 2.0).max(4.0)
        };
        let width = anchor.w.max(120.0).min(viewport.w - 8.0);
        let x = anchor.x.min((viewport.w - width - 4.0).max(4.0));

        let mut list = clover_ui::node::column()
            .position(x, y)
            .width(clover_ui::style::Length::Px(width))
            .height(clover_ui::style::Length::Px(height))
            .padding(clover_core::Edges::all(PADDING))
            .background(clover_core::Color::rgb(0x2B, 0x31, 0x38))
            .radius(4.0)
            .scrollable();

        let mut row_ids = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let highlighted = selected != usize::MAX
                && index == self.menu_highlight.min(items.len().saturating_sub(1));
            let mut row = clover_ui::node::panel()
                .width(clover_ui::style::Length::Fill)
                .height(clover_ui::style::Length::Px(ROW_HEIGHT))
                .padding(clover_core::Edges::symmetric(8.0, 0.0))
                .background(if highlighted {
                    clover_core::Color::rgb(0x3B, 0x6F, 0xD4)
                } else {
                    clover_core::Color::TRANSPARENT
                });
            row.style.cursor = clover_ui::style::Cursor::Pointer;
            row.style.ellipsis = true;
            // A trailing '>' marks a row that opens a submenu: it reports the
            // choice but keeps the menu open so the host can cascade.
            let action = if kind == "@menu" && item.trim_end().ends_with('>') {
                format!("@submenu:{name}:{index}")
            } else {
                format!("{kind}:{name}:{index}")
            };
            row.action = Some(action);
            let mut label = clover_ui::node::text(item.clone());
            label.style.ellipsis = true;
            label.style.text_color = if highlighted {
                clover_core::Color::WHITE
            } else {
                clover_core::Color::rgb(0xC8, 0xC8, 0xC8)
            };
            row.children.push(label);
            row_ids.push((row.id, index));
            list.children.push(row);
        }
        let _ = selected;

        let mut wrapper = clover_ui::node::panel()
            .width(clover_ui::style::Length::Fill)
            .height(clover_ui::style::Length::Fill)
            .hit_test(false);
        wrapper.children.push(list);
        self.menu_rows = row_ids;
        self.tree.set_overlay(Some(wrapper));
    }

    /// Whether a drag gesture is in progress.
    ///
    /// Hosts use this to swap the cursor and to stop hit-testing underneath.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Pops the next action raised by input, if any.
    pub fn take_action(&mut self) -> Option<String> {
        if self.pending_actions.is_empty() {
            None
        } else {
            Some(self.pending_actions.remove(0))
        }
    }

    /// Queues an action, for hosts that drive the UI themselves.
    pub fn push_action(&mut self, action: impl Into<String>) {
        self.pending_actions.push(action.into());
    }

    /// Routes an input event and returns the node chain, capture-first.
    pub fn dispatch(&mut self, event: UiEvent) -> Vec<RouteEntry> {
        match event {
            UiEvent::Pointer(p) => {
                // A click inside an embedded document belongs to that document.
                // Forwarding it (instead of hit-testing the page) is what makes a
                // panel a panel rather than a picture of one.
                if let Some((source, bounds)) = self.content_at(p.position) {
                    let local = clover_core::Point::new(p.position.x - bounds.x, p.position.y - bounds.y);
                    if let Some(embed) = self.embeds.get_mut(&source) {
                        let actions = embed.dispatch_pointer(p.action, local);
                        self.pending_actions.extend(actions);
                        let node_bounds = bounds;
                        self.tree.damage().add(node_bounds);
                        return Vec::new();
                    }
                }
                self.tree.update_hover(Some(p.position));
                match p.action {
                    clover_ui::PointerAction::Down => {
                        // Focus the innermost focusable ancestor of the hit node.
                        let chain = self.tree.hit_chain(p.position);
                        if self.combo.is_some() && !self.hit_is_inside_popup(&chain) {
                            self.close_combo();
                        }
                        // A drag handle claims the gesture first: dragging a panel
                        // must not also press whatever is drawn on top of it.
                        if let Some(id) = chain.iter().copied().find_map(|id| self.tree.find_widget(id)) {
                            if self.begin_widget(id, p.position) {
                                return Vec::new();
                            }
                        }
                        if let Some(target) = chain
                            .iter()
                            .copied()
                            .find_map(|id| self.tree.find_drag_ancestor(id).map(|(node, payload)| (node, payload.to_string())))
                        {
                            self.drag = Some(DragState {
                                node: target.0,
                                payload: target.1.clone(),
                                last: p.position,
                                total: clover_core::Point::default(),
                            });
                            self.pending_actions.push(format!("drag:begin:{}", target.1));
                        }
                        let focus_target = chain
                            .iter()
                            .copied()
                            .find(|id| self.tree.node(*id).map(|n| n.focusable).unwrap_or(false));
                        self.tree.set_focus(focus_target);
                        self.tree.set_pressed(chain.first().copied());
                        self.press_action = self.action_target(&chain).map(|(id, _)| id);
                    }
                    clover_ui::PointerAction::Up => {
                        if self.splitter.take().is_some() {
                            return Vec::new();
                        }
                        if let Some(id) = self.widget_drag.take() {
                            self.emit_widget_change(id);
                            return Vec::new();
                        }
                        if let Some(id) = self.widget_press.take() {
                            let chain = self.tree.hit_chain(p.position);
                            if chain.iter().any(|hit| *hit == id) {
                                self.activate_widget(id);
                            }
                            return Vec::new();
                        }
                        if let Some(drag) = self.drag.take() {
                            self.pending_actions.push(format!("drag:end:{}", drag.payload));
                            self.press_action = None;
                            self.tree.set_pressed(None);
                            return Vec::new();
                        }
                        // Activation requires press and release on the same
                        // actionable node, like every desktop toolkit.
                        let chain = self.tree.hit_chain(p.position);
                        let release = self.action_target(&chain);
                        if let (Some(pressed), Some((released, action))) = (self.press_action, release) {
                            if pressed == released {
                                if action.starts_with('@') {
                                    // Framework-internal command, e.g. a combo item.
                                    self.handle_internal(&action);
                                } else {
                                    self.pending_actions.push(action);
                                }
                            }
                        }
                        self.press_action = None;
                        self.tree.set_pressed(None);
                    }
                    clover_ui::PointerAction::Cancel => {
                        if self.splitter.take().is_some() {
                            return Vec::new();
                        }
                        if let Some(id) = self.widget_drag.take() {
                            self.emit_widget_change(id);
                            return Vec::new();
                        }
                        if let Some(id) = self.widget_press.take() {
                            let chain = self.tree.hit_chain(p.position);
                            if chain.iter().any(|hit| *hit == id) {
                                self.activate_widget(id);
                            }
                            return Vec::new();
                        }
                        if let Some(drag) = self.drag.take() {
                            self.pending_actions.push(format!("drag:end:{}", drag.payload));
                        }
                        self.press_action = None;
                        self.tree.set_pressed(None);
                    }
                    clover_ui::PointerAction::Move => {
                        if !self.menu_rows.is_empty() {
                            let chain = self.tree.hit_chain(p.position);
                            if let Some((_, row)) = self
                                .menu_rows
                                .iter()
                                .find(|(node, _)| chain.iter().any(|hit| hit == node))
                            {
                                if *row != self.menu_highlight {
                                    self.menu_highlight = *row;
                                    self.refresh_popup();
                                }
                            }
                        }
                        if let Some((id, start, origin)) = self.splitter {
                            self.drag_splitter(id, start, origin, p.position);
                        }
                        if let Some(id) = self.widget_drag {
                            self.drag_widget_to(id, p.position);
                        }
                        if let Some(drag) = self.drag.as_mut() {
                            let dx = p.position.x - drag.last.x;
                            let dy = p.position.y - drag.last.y;
                            if dx != 0.0 || dy != 0.0 {
                                drag.last = p.position;
                                drag.total.x += dx;
                                drag.total.y += dy;
                                let action = format!("drag:move:{}:{}:{}", drag.payload, dx, dy);
                                self.pending_actions.push(action);
                            }
                        }
                    }
                }
            }
            UiEvent::Wheel(w) => {
                if let Some((source, bounds)) = self.content_at(w.position) {
                    let local = clover_core::Point::new(w.position.x - bounds.x, w.position.y - bounds.y);
                    if let Some(embed) = self.embeds.get_mut(&source) {
                        embed.dispatch_wheel(w.delta, local);
                        self.tree.damage().add(bounds);
                        return Vec::new();
                    }
                }
                // Scroll the nearest scrollable ancestor of what is under the
                // pointer. Positive delta is wheel-up, which moves content down.
                let chain = self.tree.hit_chain(w.position);
                let target = chain.iter().copied().find(|id| {
                    self.tree.node(*id).map(|n| n.scroll.enabled).unwrap_or(false)
                });
                if let Some(id) = target {
                    const STEP: f32 = 40.0;
                    let mut moved = false;
                    let mut bounds = Rect::ZERO;
                    if let Some(node) = self.tree.node_mut(id) {
                        let max = node.scroll.max_offset(node.bounds.h);
                        let next = (node.scroll.offset - w.delta * STEP).clamp(0.0, max);
                        if (next - node.scroll.offset).abs() > 0.01 {
                            node.scroll.offset = next;
                            moved = true;
                        }
                        bounds = node.bounds;
                    }
                    if moved {
                        self.tree.damage().add(bounds);
                        // Wheel scrolling moves content during layout, same as the
                        // scrollbar; without this the offset changes but nothing moves.
                        self.tree.invalidate_layout();
                    }
                }
            }
            UiEvent::Key(k) => {
                if k.action == clover_ui::KeyAction::Down && !self.menu_rows.is_empty() {
                    // Arrow keys and Enter drive the open popup before anything else.
                    if k.key == 0x26 || k.key == 0x28 {
                        let count = self.menu_rows.len();
                        if k.key == 0x26 {
                            self.menu_highlight = self.menu_highlight.saturating_sub(1);
                        } else {
                            self.menu_highlight = (self.menu_highlight + 1).min(count - 1);
                        }
                        self.refresh_popup();
                        return Vec::new();
                    }
                    if k.key == 0x0D {
                        if let Some((_, index)) = self.menu_rows.get(self.menu_highlight).copied() {
                            if let Some((_, name)) = self.combo.clone() {
                                let action = format!("@combo:{name}:{index}");
                                self.handle_internal(&action);
                            }
                        }
                        return Vec::new();
                    }
                }
                if k.action == clover_ui::KeyAction::Down && k.key == 0x1B {
                    // Escape dismisses a popup before anything else sees the key.
                    if self.combo.is_some() || self.tree.overlay().is_some() {
                        self.close_combo();
                        return Vec::new();
                    }
                }
                if k.action == clover_ui::KeyAction::Down {
                    // Tab / Shift+Tab move keyboard focus before routing.
                    if k.key == 0x09 && !k.repeat {
                        self.tree.focus_next(k.modifiers.shift());
                    }
                    let editing = self
                        .tree
                        .focus()
                        .map(|id| matches!(self.tree.node(id).map(|n| &n.view), Some(View::TextInput { .. })))
                        .unwrap_or(false);
                    if editing {
                        if let Some(focused) = self.tree.focus() {
                            self.edit_text_input(focused, k.key, k.modifiers);
                        }
                        return clover_ui::event::route(&self.tree, &event);
                    }
                    // Enter and Space activate whatever has focus.
                    if k.key == 0x0D || k.key == 0x20 {
                        let action = self
                            .tree
                            .focus()
                            .and_then(|id| self.tree.root().find_action_ancestor(id))
                            .map(|(_, action)| action.to_string());
                        if let Some(action) = action {
                            self.pending_actions.push(action);
                        }
                    }
                }
            }
        }
        clover_ui::event::route(&self.tree, &event)
    }
}

enum StyleTarget {
    Opacity(f32),
    Background(clover_core::Color),
}

/// Collects embedded documents and the physical size their boxes ask for.
fn collect_content(node: &Node, scale: f32, out: &mut Vec<(String, u32, u32)>) {
    if !node.visible {
        return;
    }
    if let View::Content { source } = &node.view {
        let box_rect = node.style.padding.deflate(node.bounds).scale(scale);
        out.push((
            source.clone(),
            box_rect.w.round().max(1.0) as u32,
            box_rect.h.round().max(1.0) as u32,
        ));
        return;
    }
    for child in &node.children {
        collect_content(child, scale, out);
    }
}

fn advance_node(node: &mut Node, now_ms: u64, changed: &mut Vec<Rect>, active: &mut bool) {
    if let Some(animation) = node.animation {
        let progress = animation.progress(now_ms);
        node.style.opacity = animation.from_opacity
            + (animation.to_opacity - animation.from_opacity) * progress;
        node.style.background = animation.from_background.lerp(animation.to_background, progress);
        changed.push(node.bounds);
        if animation.is_finished(now_ms) {
            node.animation = None;
        } else {
            *active = true;
        }
    }
    for child in &mut node.children {
        advance_node(child, now_ms, changed, active);
    }
}

/// An in-progress drag. The framework owns the gesture; the host owns the meaning.
struct DragState {
    #[allow(dead_code)]
    node: NodeId,
    payload: String,
    last: clover_core::Point,
    total: clover_core::Point,
}

fn previous_boundary(text: &str, position: usize) -> usize {
    let mut index = position.saturating_sub(1);
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_boundary(text: &str, position: usize) -> usize {
    let mut index = (position + 1).min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn physical_size(width: f32, height: f32, scale: f32) -> (u32, u32) {
    let scale = scale.max(0.01);
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clover_core::{Color, Point};
    use clover_ui::{node, style::Length, PointerAction, PointerEvent, UiEvent};

    fn surface_with_button() -> Surface {
        let root = node::panel()
            .background(Color::BLACK)
            .child(
                node::panel()
                    .size(Length::Px(20.0), Length::Px(20.0))
                    .background(Color::rgb(0x40, 0x40, 0x40))
                    .name("btn"),
            );
        let mut surface = Surface::new(root, 100.0, 100.0, 2.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface
    }

    #[test]
    fn frame_is_physical_size() {
        let surface = surface_with_button();
        assert_eq!((surface.canvas().width, surface.canvas().height), (200, 200));
    }

    #[test]
    fn typing_edits_the_focused_text_field() {
        let root = node::column()
            .background(Color::BLACK)
            .child(node::text_input("", "Search").name("search").width(Length::Px(150.0)));
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.tree_mut().focus_next(false);

        let press = |surface: &mut Surface, key: u32| {
            surface.dispatch(UiEvent::Key(clover_ui::KeyEvent {
                action: clover_ui::KeyAction::Down,
                key,
                modifiers: clover_ui::Modifiers::NONE,
                repeat: false,
            }));
        };
        press(&mut surface, 'A' as u32);
        press(&mut surface, 'B' as u32);
        assert_eq!(surface.text_of("search").as_deref(), Some("ab"));
        // One change event per keystroke.
        assert_eq!(surface.take_action().as_deref(), Some("text:search"));
        assert_eq!(surface.take_action().as_deref(), Some("text:search"));
        assert_eq!(surface.take_action(), None);

        press(&mut surface, 0x08); // backspace
        assert_eq!(surface.text_of("search").as_deref(), Some("a"));

        press(&mut surface, 0x24); // home
        press(&mut surface, 'X' as u32);
        assert_eq!(surface.text_of("search").as_deref(), Some("xa"));

        while surface.take_action().is_some() {}
        press(&mut surface, 0x0D); // enter
        assert_eq!(surface.take_action().as_deref(), Some("submit:search"));
    }

    fn drag_event(action: PointerAction, x: f32, y: f32) -> UiEvent {
        UiEvent::Pointer(PointerEvent {
            action,
            position: Point::new(x, y),
            button: clover_ui::MouseButton::Left,
            ..Default::default()
        })
    }

    #[test]
    fn overlay_wins_the_hit_test_and_paints_last() {
        let page = clover_ui::xaml::parse(
            r##"<StackPanel Background="#101010"><Panel Width="Fill" Height="Fill" x:Name="under"/></StackPanel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(page, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        let menu = clover_ui::xaml::parse(
            r##"<Panel Position="20,20" Width="100" Height="60" Background="#303030" x:Name="menu"/>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        surface.set_overlay(Some(menu));
        surface.invalidate_all();
        let _ = surface.frame();

        let overlay_id = surface.tree().overlay().map(|node| node.id);
        assert!(overlay_id.is_some(), "overlay was not attached");
        let chain = surface.tree().hit_chain(Point::new(40.0, 40.0));
        assert_eq!(
            chain.last().copied(),
            overlay_id,
            "the overlay root must be the outermost hit, not the page beneath it"
        );
    }

    #[test]
    fn cursor_follows_the_pointer() {
        let page = clover_ui::xaml::parse(
            r#"<StackPanel><Button Content="x" Cursor="pointer" Width="80" Height="40"/></StackPanel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(page, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        assert_eq!(surface.cursor_at(40.0, 20.0), clover_ui::style::Cursor::Pointer);
        assert_eq!(surface.cursor_at(180.0, 180.0), clover_ui::style::Cursor::Default);
    }

    #[test]
    fn slider_drag_reports_the_value() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><Slider x:Name="size" Value="0" Min="0" Max="100" Width="200" Height="20"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 100.0, 10.0));
        // The track reserves 34px for the readout, so 100px of 166 is ~60%.
        let value = match surface.tree().named("size").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::Slider { value, .. }) => value,
            other => panic!("expected a slider, got {other:?}"),
        };
        assert!(value > 50.0 && value < 70.0, "value should follow the pointer, got {value}");

        surface.dispatch(drag_event(PointerAction::Move, 200.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 200.0, 10.0));
        let action = surface.take_action().expect("a change action");
        assert!(action.starts_with("change:size:"), "got {action}");
    }

    #[test]
    fn checkbox_click_toggles_and_reports() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><CheckBox x:Name="lock" IsChecked="false"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 100.0, 40.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 7.0, 7.0));
        surface.dispatch(drag_event(PointerAction::Up, 7.0, 7.0));
        let checked = match surface.tree().named("lock").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::CheckBox { checked }) => checked,
            other => panic!("expected a checkbox, got {other:?}"),
        };
        assert!(checked, "the box should be checked");
        assert_eq!(surface.take_action().as_deref(), Some("change:lock:1"));
    }

    #[test]
    fn a_bound_direct_surface_is_composited_into_the_page() {
        let root = clover_ui::xaml::parse(
            r##"<StackPanel Background="#202020" Width="Fill" Height="Fill">
                  <Panel Height="40" Background="#303030"/>
                  <CloverSurface x:Name="canvas" Width="Fill" Height="Fill"/>
                </StackPanel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        let region = DirectSurface::new(200, 160);
        region.write(|pixels, stride, rows| {
            for y in 0..rows as usize {
                for x in 0..stride as usize {
                    pixels[y * stride as usize + x] = 0xFF30_D050;
                }
            }
        });
        let handle = SurfaceHandle(7);
        surface.register_direct(handle, Arc::clone(&region));
        assert!(surface.bind_direct_surface("canvas", handle), "the node must bind");
        surface.invalidate_all();
        let _ = surface.frame();

        let pixel = surface.renderer().canvas().pixel(100, 120);
        assert_eq!(pixel.r(), 0x30, "the canvas pixels reach the frame");
        assert_eq!(pixel.g(), 0xD0);
    }

    #[test]
    fn set_value_pushes_into_the_widget() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><Slider x:Name="size" Value="0" Min="0" Max="100" Width="200"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.set_value("size", 42.0));
        assert!(!surface.set_value("size", 42.0), "an unchanged value is not a change");
        let value = match surface.tree().named("size").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::Slider { value, .. }) => value,
            other => panic!("expected a slider, got {other:?}"),
        };
        assert_eq!(value, 42.0);
        assert!(surface.set_value("size", 500.0), "values clamp to the range");
    }

    #[test]
    fn set_checked_and_selected_push_into_widgets() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><CheckBox x:Name="lock"/><ComboBox x:Name="blend" Items="A,B,C"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.set_checked("lock", true));
        assert!(surface.set_selected("blend", 2));
        assert!(matches!(
            surface.tree().named("lock").and_then(|node| node.widget.clone()),
            Some(clover_ui::node::Widget::CheckBox { checked: true })
        ));
        assert!(matches!(
            surface.tree().named("blend").and_then(|node| node.widget.clone()),
            Some(clover_ui::node::Widget::ComboBox { selected: 2, .. })
        ));
    }

    #[test]
    fn disabled_nodes_ignore_input() {
        let root = clover_ui::xaml::parse(
            r##"<Panel><Panel x:Name="btn" Width="80" Height="40" Background="#333333" Command="click:btn"/></Panel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        assert!(surface.set_enabled("btn", false));
        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));
        assert_eq!(surface.take_action(), None, "a disabled node must not fire");
    }

    #[test]
    fn splitter_resizes_its_target() {
        let root = clover_ui::xaml::parse(
            r##"<StackPanel Orientation="Horizontal" Width="Fill" Height="Fill">
                 <Panel x:Name="left" Width="200" Height="Fill" Background="#202020"/>
                 <Splitter x:Name="split" Orientation="Vertical" Target="left" Min="120" Max="400" Width="4" Height="Fill"/>
                 <Panel Width="Fill" Height="Fill" Background="#303030"/>
               </StackPanel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 600.0, 300.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        assert_eq!(surface.tree().named("left").unwrap().bounds.w, 200.0);

        surface.dispatch(drag_event(PointerAction::Down, 202.0, 100.0));
        surface.dispatch(drag_event(PointerAction::Move, 262.0, 100.0));
        surface.dispatch(drag_event(PointerAction::Up, 262.0, 100.0));
        surface.invalidate_all();
        let _ = surface.frame();

        let width = surface.tree().named("left").unwrap().bounds.w;
        assert!((width - 260.0).abs() < 1.0, "the pane should widen by the drag, got {width}");
    }

    #[test]
    fn escape_dismisses_the_popup() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><ComboBox x:Name="blend" Width="120" Height="20" Items="A,B"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));
        assert!(surface.tree().overlay().is_some());
        surface.dispatch(UiEvent::Key(clover_ui::KeyEvent {
            key: 0x1B,
            action: clover_ui::KeyAction::Down,
            modifiers: clover_ui::Modifiers::default(),
            repeat: false,
        }));
        assert!(surface.tree().overlay().is_none(), "Esc closes the popup");
    }

    #[test]
    fn combo_opens_a_popup_and_selects_an_item() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><ComboBox x:Name="blend" Width="120" Height="20" Items="Normal,Multiply,Screen" SelectedIndex="0"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));
        assert!(surface.tree().overlay().is_some(), "the popup lives in the overlay");
        assert_eq!(surface.take_action(), None, "opening is not a change");
        // The popup is laid out on the next frame, exactly like a real host.
        surface.invalidate_all();
        let _ = surface.frame();

        // Row 1 sits below the box: box 20px + 2px gap + 2px padding + 20px row.
        surface.dispatch(drag_event(PointerAction::Down, 10.0, 54.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 54.0));

        let selected = match surface.tree().named("blend").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::ComboBox { selected, .. }) => selected,
            other => panic!("expected a combo, got {other:?}"),
        };
        assert_eq!(selected, 1, "clicking row 1 selects index 1");
        assert!(surface.tree().overlay().is_none(), "choosing closes the popup");
        assert_eq!(surface.take_action().as_deref(), Some("change:blend:1"));
    }

    #[test]
    fn popup_flips_above_when_there_is_no_room_below() {
        let markup = format!(
            r##"<Panel><ComboBox x:Name="blend" Position="10,150" Width="120" Height="20"
                     Items="{}"/></Panel>"##,
            "A,B,C,D,E,F,G,H,I,J,K,L,M,N,O,P"
        );
        let root = clover_ui::xaml::parse(&markup, &clover_ui::xaml::NoImages).unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Down, 20.0, 155.0));
        surface.dispatch(drag_event(PointerAction::Up, 20.0, 155.0));

        let overlay = surface.tree().overlay().expect("popup");
        let list = &overlay.children[0];
        assert!(
            list.bounds.bottom() <= 200.0,
            "the list must stay inside the viewport, got {:?}",
            list.bounds
        );
        assert!(
            list.bounds.y < 150.0,
            "with no room below it flips above the anchor, got {:?}",
            list.bounds
        );
    }

    #[test]
    fn popup_is_clamped_to_the_viewport_width() {
        let root = clover_ui::xaml::parse(
            r##"<Panel><ComboBox x:Name="blend" Position="150,10" Width="120" Height="20" Items="A,B"/></Panel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Down, 160.0, 15.0));
        surface.dispatch(drag_event(PointerAction::Up, 160.0, 15.0));

        let overlay = surface.tree().overlay().expect("popup");
        let list = &overlay.children[0];
        assert!(list.bounds.right() <= 200.0, "popup must not run off the right edge");
    }

    #[test]
    fn arrow_keys_move_the_highlight_and_enter_selects() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><ComboBox x:Name="blend" Width="120" Height="20" Items="A,B,C"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));

        let key = |code: u32| {
            UiEvent::Key(clover_ui::KeyEvent {
                key: code,
                action: clover_ui::KeyAction::Down,
                modifiers: clover_ui::Modifiers::default(),
                repeat: false,
            })
        };
        surface.dispatch(key(0x28));
        surface.dispatch(key(0x28));
        surface.dispatch(key(0x0D));

        let selected = match surface.tree().named("blend").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::ComboBox { selected, .. }) => selected,
            other => panic!("expected a combo, got {other:?}"),
        };
        assert_eq!(selected, 2, "two downs then enter selects the third item");
        assert_eq!(surface.take_action().as_deref(), Some("change:blend:2"));
        assert!(surface.tree().overlay().is_none(), "choosing closes the popup");
    }

    #[test]
    fn an_embedded_document_is_composited_into_its_box() {
        let dir = std::env::temp_dir().join("clover-embed-test");
        let _ = std::fs::create_dir_all(&dir);
        let child = dir.join("child.cui");
        std::fs::write(
            &child,
            r##"<Panel Background="#20C040" Width="Fill" Height="Fill"/>"##,
        )
        .unwrap();

        let markup = format!(
            r##"<StackPanel Background="#101010" Width="Fill" Height="Fill">
                  <Panel Height="40" Background="#303030"/>
                  <Content x:Name="slot" Source="{}"/>
                </StackPanel>"##,
            child.display()
        );
        let root = clover_ui::xaml::parse(&markup, &clover_ui::xaml::NoImages).unwrap();
        let mut surface = Surface::new(root, 120.0, 120.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        let inside = surface.renderer().canvas().pixel(60, 80);
        assert_eq!(inside.g(), 0xC0, "the child's background reaches the parent");
        let header = surface.renderer().canvas().pixel(60, 10);
        assert_eq!(header.r(), 0x30, "the parent's own chrome is untouched");
    }

    #[test]
    fn a_broken_embedded_document_does_not_break_the_parent() {
        let root = clover_ui::xaml::parse(
            r##"<StackPanel Background="#101010" Width="Fill" Height="Fill">
                  <Content x:Name="slot" Source="/definitely/not/here.cui"/>
                </StackPanel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 80.0, 80.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        // The failed child is marked, the page still paints.
        let pixel = surface.renderer().canvas().pixel(40, 40);
        assert!(pixel.a() > 0, "the parent still rendered something");
    }

    #[test]
    fn a_click_inside_a_slot_reaches_the_child() {
        let dir = std::env::temp_dir().join("clover-embed-test");
        let _ = std::fs::create_dir_all(&dir);
        let child = dir.join("button.cui");
        std::fs::write(
            &child,
            r##"<Panel Background="#202020" Width="Fill" Height="Fill">
                  <Panel x:Name="go" Width="60" Height="30" Background="#3B6FD4" Command="child:go"/>
                </Panel>"##,
        )
        .unwrap();

        let markup = format!(
            r##"<Panel Background="#000000" Width="Fill" Height="Fill"><Content x:Name="slot" Source="{}"/></Panel>"##,
            child.display()
        );
        let root = clover_ui::xaml::parse(&markup, &clover_ui::xaml::NoImages).unwrap();
        let mut surface = Surface::new(root, 80.0, 80.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 20.0, 15.0));
        surface.dispatch(drag_event(PointerAction::Up, 20.0, 15.0));
        assert_eq!(surface.take_action().as_deref(), Some("child:go"));
    }

    #[test]
    fn slot_addressing_reaches_controls_inside_a_child() {
        let dir = std::env::temp_dir().join("clover-embed-test");
        let _ = std::fs::create_dir_all(&dir);
        let child = dir.join("slider.cui");
        std::fs::write(
            &child,
            r##"<Panel Background="#202020" Width="Fill" Height="Fill">
                  <Slider x:Name="amount" Value="0" Min="0" Max="100" Width="100" Height="20"/>
                </Panel>"##,
        )
        .unwrap();

        let markup = format!(
            r##"<Panel Background="#000000" Width="Fill" Height="Fill"><Content x:Name="slot" Source="{}"/></Panel>"##,
            child.display()
        );
        let root = clover_ui::xaml::parse(&markup, &clover_ui::xaml::NoImages).unwrap();
        let mut surface = Surface::new(root, 160.0, 80.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.set_value("slot:amount", 60.0), "the slot namespace reaches the child");
        surface.invalidate_all();
        let _ = surface.frame();
        let embed = surface.embeds.values().next().expect("one embedded document");
        let value = match embed.tree.named("amount").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::Slider { value, .. }) => value,
            other => panic!("expected a slider, got {other:?}"),
        };
        assert_eq!(value, 60.0);
    }

    #[test]
    fn set_content_swaps_what_the_slot_shows() {
        let dir = std::env::temp_dir().join("clover-embed-test");
        let _ = std::fs::create_dir_all(&dir);
        let first = dir.join("first.cui");
        let second = dir.join("second.cui");
        std::fs::write(&first, r##"<Panel Background="#2040C0" Width="Fill" Height="Fill"/>"##).unwrap();
        std::fs::write(&second, r##"<Panel Background="#C04020" Width="Fill" Height="Fill"/>"##).unwrap();

        let markup = format!(
            r##"<Panel Background="#000000" Width="Fill" Height="Fill">
                  <Content x:Name="slot" Source="{}"/>
                </Panel>"##,
            first.display()
        );
        let root = clover_ui::xaml::parse(&markup, &clover_ui::xaml::NoImages).unwrap();
        let mut surface = Surface::new(root, 60.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        assert_eq!(surface.renderer().canvas().pixel(30, 30).b(), 0xC0);

        assert!(surface.set_content("slot", &second.display().to_string()));
        surface.invalidate_all();
        let _ = surface.frame();
        let after = surface.renderer().canvas().pixel(30, 30);
        assert_eq!(after.r(), 0xC0, "the slot now shows the second document");
    }

    #[test]
    fn a_submenu_row_keeps_the_menu_open() {
        let root = clover_ui::xaml::parse(
            r##"<Panel Background="#101010" Width="Fill" Height="Fill"/>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.open_menu(
            clover_core::Rect::new(20.0, 20.0, 120.0, 20.0),
            vec!["Delete".into(), "Merge>".into()],
            "layer"
        ));
        surface.invalidate_all();
        let _ = surface.frame();

        // The menu opens below the anchor: 20 + 20 anchor height + 2 gap, then
        // 2px padding, one 22px row, and half of the next.
        let y = 20.0 + 20.0 + 2.0 + 2.0 + 22.0 + 11.0;
        surface.dispatch(drag_event(PointerAction::Down, 60.0, y));
        surface.dispatch(drag_event(PointerAction::Up, 60.0, y));

        assert_eq!(surface.take_action().as_deref(), Some("layer:1"));
        assert!(surface.tree().overlay().is_some(), "a submenu row must not close the menu");
        assert!(surface.open_submenu(vec!["Down".into(), "Visible".into()], "merge"));
        surface.invalidate_all();
        let _ = surface.frame();
        assert!(surface.tree().overlay().is_some(), "the cascade is open");
    }

    #[test]
    fn a_context_menu_reports_the_chosen_index() {
        let root = clover_ui::xaml::parse(
            r##"<Panel Background="#101010" Width="Fill" Height="Fill"/>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.open_menu(
            clover_core::Rect::new(40.0, 40.0, 120.0, 20.0),
            vec!["Duplicate".into(), "Delete".into()],
            "layer-menu"
        ));
        surface.invalidate_all();
        let _ = surface.frame();
        assert!(surface.tree().overlay().is_some(), "the menu is an overlay");

        // Second row sits 22px under the first, which starts 2px below the anchor.
        surface.dispatch(drag_event(PointerAction::Down, 60.0, 40.0 + 22.0 + 4.0 + 22.0 + 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 60.0, 40.0 + 22.0 + 4.0 + 22.0 + 10.0));
        assert!(surface.tree().overlay().is_none(), "choosing closes the menu");
        assert_eq!(surface.take_action().as_deref(), Some("layer-menu:1"));
    }

    #[test]
    fn set_items_rebuilds_the_open_popup() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><ComboBox x:Name="blend" Width="120" Height="20" Items="A,B"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));

        assert!(surface.set_items("blend", vec!["X".into(), "Y".into(), "Z".into(), "W".into()]));
        surface.invalidate_all();
        let _ = surface.frame();
        let overlay = surface.tree().overlay().expect("popup");
        let rows = overlay.children[0].children.len();
        assert_eq!(rows, 4, "the open list follows the new items");
    }

    #[test]
    fn clicking_outside_closes_the_combo_popup() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><ComboBox x:Name="blend" Width="120" Height="20" Items="A,B"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));
        assert!(surface.tree().overlay().is_some());
        surface.dispatch(drag_event(PointerAction::Down, 180.0, 180.0));
        assert!(surface.tree().overlay().is_none(), "a click elsewhere dismisses it");
    }

    #[test]
    fn wheel_scroll_moves_content_after_layout() {
        let root = clover_ui::xaml::parse(
            r##"<ScrollViewer x:Name="list" Width="200" Height="100">
                  <StackPanel Width="Fill">
                    <Panel Height="120" Background="#202020"/>
                    <Panel Height="120" Background="#262626"/>
                  </StackPanel>
                </ScrollViewer>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        let before = surface.tree().named("list").unwrap().children[0].children[0].bounds.y;
        surface.dispatch(UiEvent::Wheel(clover_ui::WheelEvent {
            position: Point::new(100.0, 50.0),
            delta: -2.0,
            modifiers: clover_ui::Modifiers::default(),
        }));
        surface.invalidate_all();
        let _ = surface.frame();
        let after = surface.tree().named("list").unwrap().children[0].children[0].bounds.y;
        assert!(after < before, "content should move up, {before} -> {after}");
    }

    #[test]
    fn scrollbar_drag_moves_the_container_it_targets() {
        let root = clover_ui::xaml::parse(
            r##"<StackPanel Width="Fill" Height="Fill">
                  <ScrollViewer x:Name="list" Width="Fill" Height="200">
                    <StackPanel Width="Fill">
                      <Panel Height="120" Background="#202020"/>
                      <Panel Height="120" Background="#262626"/>
                      <Panel Height="120" Background="#2C2C2C"/>
                    </StackPanel>
                  </ScrollViewer>
                  <ScrollBar x:Name="listbar" Orientation="Vertical" Target="list"
                             Offset="0" Viewport="200" Extent="360" Width="10" Height="200"/>
                </StackPanel>"##,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 200.0, 400.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        let bar = surface.tree().named("listbar").unwrap().bounds;
        surface.dispatch(drag_event(PointerAction::Down, bar.x + 5.0, bar.y + bar.h * 0.8));
        surface.dispatch(drag_event(PointerAction::Up, bar.x + 5.0, bar.y + bar.h * 0.8));

        let offset = surface.tree().named("list").unwrap().scroll.offset;
        assert!(offset > 50.0, "the container should have scrolled, got {offset}");
    }

    #[test]
    fn scrollbar_drag_moves_the_offset() {
        let root = clover_ui::xaml::parse(
            r#"<Panel><ScrollBar x:Name="bar" Orientation="Vertical" Offset="0" Viewport="100" Extent="400" Width="10" Height="100"/></Panel>"#,
            &clover_ui::xaml::NoImages,
        )
        .unwrap();
        let mut surface = Surface::new(root, 40.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 5.0, 50.0));
        surface.dispatch(drag_event(PointerAction::Up, 5.0, 50.0));
        let offset = match surface.tree().named("bar").and_then(|node| node.widget.clone()) {
            Some(clover_ui::node::Widget::ScrollBar { offset, .. }) => offset,
            other => panic!("expected a scrollbar, got {other:?}"),
        };
        assert!(offset > 100.0, "halfway down a 400px extent should pass 100, got {offset}");
        assert!(surface.take_action().is_some(), "dragging reports an action");
    }

    #[test]
    fn dragging_reports_begin_move_end_with_deltas() {
        let root = node::panel().child(
            node::panel()
                .size(Length::Px(60.0), Length::Px(60.0))
                .drag("panel")
                .name("handle"),
        );
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        assert_eq!(surface.take_action().as_deref(), Some("drag:begin:panel"));
        assert!(surface.is_dragging());

        surface.dispatch(drag_event(PointerAction::Move, 14.0, 13.0));
        assert_eq!(surface.take_action().as_deref(), Some("drag:move:panel:4:3"));

        surface.dispatch(drag_event(PointerAction::Move, 20.0, 20.0));
        assert_eq!(surface.take_action().as_deref(), Some("drag:move:panel:6:7"));

        surface.dispatch(drag_event(PointerAction::Up, 20.0, 20.0));
        assert_eq!(surface.take_action().as_deref(), Some("drag:end:panel"));
        assert!(!surface.is_dragging());
    }

    #[test]
    fn a_drag_does_not_also_fire_the_action() {
        let root = node::panel().child(
            node::panel()
                .size(Length::Px(60.0), Length::Px(60.0))
                .drag("panel")
                .action("select")
                .name("handle"),
        );
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(drag_event(PointerAction::Down, 10.0, 10.0));
        surface.dispatch(drag_event(PointerAction::Up, 10.0, 10.0));
        let mut seen = Vec::new();
        while let Some(action) = surface.take_action() {
            seen.push(action);
        }
        assert_eq!(seen, vec!["drag:begin:panel", "drag:end:panel"], "the click action must not also fire");
    }

    #[test]
    fn a_move_without_a_drag_is_silent() {
        let root = node::panel().child(node::panel().size(Length::Px(60.0), Length::Px(60.0)));
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.dispatch(drag_event(PointerAction::Move, 60.0, 60.0));
        assert_eq!(surface.take_action(), None);
    }

    #[test]
    fn animation_eases_towards_the_target_and_finishes() {
        let root = node::panel().child(node::panel().name("fade").background(Color::BLACK));
        let mut surface = Surface::new(root, 100.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.animate_opacity("fade", 0.0, 100, 1_000));
        assert!(surface.advance(1_000), "animation is running at t=0");
        let start = surface.tree().named("fade").unwrap().style.opacity;
        assert!((start - 1.0).abs() < 0.01);

        assert!(surface.advance(1_050), "still running at the midpoint");
        let mid = surface.tree().named("fade").unwrap().style.opacity;
        assert!(mid < 1.0 && mid > 0.0, "eased value should be between, got {mid}");

        assert!(!surface.advance(1_200), "finished after the duration");
        let end = surface.tree().named("fade").unwrap().style.opacity;
        assert!(end.abs() < 0.001, "lands exactly on the target, got {end}");
        assert!(surface.tree().named("fade").unwrap().animation.is_none());
    }

    #[test]
    fn background_animation_interpolates_colour() {
        let root = node::panel().child(node::panel().name("tint").background(Color::BLACK));
        let mut surface = Surface::new(root, 100.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        assert!(surface.animate_background("tint", Color::WHITE, 100, 0));
        surface.advance(50);
        let color = surface.tree().named("tint").unwrap().style.background;
        assert!(color.r > 100 && color.r < 255, "mid-way between black and white, got {color:?}");
    }

    #[test]
    fn animating_an_unknown_node_is_a_no_op() {
        let root = node::panel();
        let mut surface = Surface::new(root, 100.0, 100.0, 1.0).expect("system font required");
        assert!(!surface.animate_opacity("missing", 0.5, 100, 0));
        assert!(!surface.advance(10));
    }

    #[test]
    fn ime_composition_is_preview_then_committed() {
        let root = node::panel().child(node::text_input("", "Search").name("field"));
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.tree_mut().focus_next(false);

        assert!(surface.ime_mark("zhang"));
        assert_eq!(surface.text_of("field").as_deref(), Some(""), "composition is not committed");
        assert!(surface.focused_caret_rect().is_some(), "candidate window needs a caret rect");

        assert!(surface.ime_commit());
        assert_eq!(surface.text_of("field").as_deref(), Some("zhang"));
        assert_eq!(surface.take_action().as_deref(), Some("text:field"));
    }

    #[test]
    fn ime_cancel_drops_the_composition() {
        let root = node::panel().child(node::text_input("abc", "").name("field"));
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.tree_mut().focus_next(false);

        assert!(surface.ime_mark("xyz"));
        assert!(surface.ime_cancel());
        assert_eq!(surface.text_of("field").as_deref(), Some("abc"));
        assert_eq!(surface.take_action(), None, "cancelling is not a change event");
    }

    #[test]
    fn ime_insert_commits_direct_text() {
        let root = node::panel().child(node::text_input("ab", "").name("field"));
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.tree_mut().focus_next(false);

        surface.ime_mark("ignored");
        assert!(surface.ime_insert("图层"));
        assert_eq!(surface.text_of("field").as_deref(), Some("ab图层"));
        assert_eq!(surface.take_action().as_deref(), Some("text:field"));
    }

    #[test]
    fn ime_without_focus_is_a_no_op() {
        let root = node::panel().child(node::text_input("", "").name("field"));
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        assert!(!surface.ime_mark("x"));
        assert!(!surface.ime_commit());
        assert!(!surface.ime_insert("x"));
        assert!(surface.focused_caret_rect().is_none());
    }

    #[test]
    fn set_text_updates_the_buffer_and_caret() {
        let root = node::panel().child(node::text_input("old", "").name("field"));
        let mut surface = Surface::new(root, 200.0, 60.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        assert!(surface.set_text("field", "new value"));
        assert_eq!(surface.text_of("field").as_deref(), Some("new value"));
    }

    #[test]
    fn click_activates_an_actionable_node() {
        let root = node::column()
            .background(Color::BLACK)
            .child(node::button("Run").action("run").name("btn"));
        let mut surface = Surface::new(root, 200.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        let centre = surface.tree().named("btn").unwrap().bounds.center();
        let click = |surface: &mut Surface, action: PointerAction| {
            surface.dispatch(UiEvent::Pointer(PointerEvent {
                action,
                position: centre,
                button: clover_ui::MouseButton::Left,
                ..Default::default()
            }));
        };
        click(&mut surface, PointerAction::Down);
        click(&mut surface, PointerAction::Up);
        assert_eq!(surface.take_action().as_deref(), Some("run"));
        assert_eq!(surface.take_action(), None, "the queue drains");
    }

    #[test]
    fn release_outside_the_pressed_node_does_not_activate() {
        let root = node::column().background(Color::BLACK).child(
            node::panel()
                .size(Length::Px(40.0), Length::Px(40.0))
                .action("nope")
                .name("btn"),
        );
        let mut surface = Surface::new(root, 200.0, 200.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        surface.dispatch(UiEvent::Pointer(PointerEvent {
            action: PointerAction::Down,
            position: Point::new(20.0, 20.0),
            button: clover_ui::MouseButton::Left,
            ..Default::default()
        }));
        surface.dispatch(UiEvent::Pointer(PointerEvent {
            action: PointerAction::Up,
            position: Point::new(150.0, 150.0),
            button: clover_ui::MouseButton::Left,
            ..Default::default()
        }));
        assert_eq!(surface.take_action(), None);
    }

    #[test]
    fn enter_and_space_activate_the_focused_node() {
        let root = node::panel()
            .background(Color::BLACK)
            .child(node::panel().size(Length::Px(40.0), Length::Px(40.0)).action("go").focusable(true));
        let mut surface = Surface::new(root, 100.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface.tree_mut().focus_next(false);

        for key in [0x0D, 0x20] {
            surface.dispatch(UiEvent::Key(clover_ui::KeyEvent {
                action: clover_ui::KeyAction::Down,
                key,
                modifiers: clover_ui::Modifiers::NONE,
                repeat: false,
            }));
            assert_eq!(surface.take_action().as_deref(), Some("go"), "key {key:#x} activates");
        }
    }

    #[test]
    fn wheel_scrolls_the_nearest_scrollable_ancestor() {
        let root = node::panel().background(Color::BLACK).child(
            node::column()
                .size(Length::Px(100.0), Length::Px(50.0))
                .scrollable()
                .background(Color::rgb(0x30, 0x30, 0x30))
                .child(node::panel().size(Length::Fill, Length::Px(40.0)).background(Color::rgb(0xC0, 0x20, 0x20)))
                .child(node::panel().size(Length::Fill, Length::Px(40.0)).background(Color::rgb(0x20, 0xC0, 0x20)))
                .child(node::panel().size(Length::Fill, Length::Px(40.0)).background(Color::rgb(0x20, 0x20, 0xC0))),
        );
        let mut surface = Surface::new(root, 200.0, 120.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();

        let scroller = surface
            .tree()
            .root()
            .children
            .iter()
            .find(|n| n.scroll.enabled)
            .expect("scroll container");
        assert_eq!(scroller.scroll.offset, 0.0);

        // Wheel down (negative delta) over the scroll area.
        surface.dispatch(UiEvent::Wheel(clover_ui::WheelEvent {
            delta: -2.0,
            position: Point::new(50.0, 30.0),
            modifiers: clover_ui::Modifiers::NONE,
        }));
        let _ = surface.frame();
        let scroller = surface
            .tree()
            .root()
            .children
            .iter()
            .find(|n| n.scroll.enabled)
            .unwrap();
        assert!(scroller.scroll.offset > 0.0, "wheel down scrolls content up");
        assert_eq!(scroller.scroll.offset, 70.0, "2 notches * 40px, clamped to content - viewport");
    }

    #[test]
    fn hover_damage_is_converted_to_physical_pixels() {
        let mut surface = surface_with_button();
        let before = surface.canvas().pixel(20, 20).to_color();
        let outside_before = surface.canvas().pixel(150, 150).to_color();

        // Hover the 20x20 logical button at (0,0); at scale 2 that is (0,0,40,40)
        // in physical pixels.
        surface.dispatch(UiEvent::Pointer(PointerEvent {
            action: PointerAction::Move,
            position: Point::new(10.0, 10.0),
            ..Default::default()
        }));
        let _ = surface.frame();
        let after = surface.canvas().pixel(20, 20).to_color();
        assert_ne!(before, after, "hover must repaint the physical region");

        // A pixel far outside the button must be untouched by this damage.
        assert_eq!(surface.canvas().pixel(150, 150).to_color(), outside_before);
    }
}

#[cfg(test)]
mod damage_tests {
    use super::*;
    use clover_core::Color;
    use clover_ui::{node, style::Length};

    fn surface() -> Surface {
        let root = node::column()
            .background(Color::BLACK)
            .child(node::panel().size(Length::Px(40.0), Length::Px(40.0)).background(Color::WHITE).name("btn"));
        let mut surface = Surface::new(root, 200.0, 100.0, 1.0).expect("system font required");
        surface.invalidate_all();
        let _ = surface.frame();
        surface
    }

    #[test]
    fn layout_runs_once_and_is_then_clean() {
        let surface = surface();
        assert!(!surface.tree().needs_layout(), "first frame settles layout");
    }

    #[test]
    fn a_second_frame_without_changes_does_no_work() {
        let mut surface = surface();
        let generation = surface.frame().generation;
        // Nothing changed: no layout, no damage, no new frame.
        let again = surface.frame().generation;
        assert_eq!(generation, again, "an idle frame must not re-render");
    }

    #[test]
    fn hover_only_renders_the_damaged_region() {
        let mut surface = surface();
        surface.dispatch(UiEvent::Pointer(clover_ui::PointerEvent {
            action: clover_ui::PointerAction::Move,
            position: clover_core::Point::new(20.0, 20.0),
            ..Default::default()
        }));
        let _ = surface.frame();
        assert_eq!(
            surface.renderer_mut().last_tile_count(),
            1,
            "a 40x40 hover must repaint one tile, not the whole surface"
        );
    }
}
