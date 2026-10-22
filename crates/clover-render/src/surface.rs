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
                self.tree.update_hover(Some(p.position));
                match p.action {
                    clover_ui::PointerAction::Down => {
                        // Focus the innermost focusable ancestor of the hit node.
                        let chain = self.tree.hit_chain(p.position);
                        let focus_target = chain
                            .iter()
                            .copied()
                            .find(|id| self.tree.node(*id).map(|n| n.focusable).unwrap_or(false));
                        self.tree.set_focus(focus_target);
                        self.tree.set_pressed(chain.first().copied());
                        self.press_action = self.action_target(&chain).map(|(id, _)| id);
                    }
                    clover_ui::PointerAction::Up => {
                        // Activation requires press and release on the same
                        // actionable node, like every desktop toolkit.
                        let chain = self.tree.hit_chain(p.position);
                        let release = self.action_target(&chain);
                        if let (Some(pressed), Some((released, action))) = (self.press_action, release) {
                            if pressed == released {
                                self.pending_actions.push(action);
                            }
                        }
                        self.press_action = None;
                        self.tree.set_pressed(None);
                    }
                    clover_ui::PointerAction::Cancel => {
                        self.press_action = None;
                        self.tree.set_pressed(None);
                    }
                    clover_ui::PointerAction::Move => {}
                }
            }
            UiEvent::Wheel(w) => {
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
                    }
                }
            }
            UiEvent::Key(k) => {
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
