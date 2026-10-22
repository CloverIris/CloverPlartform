//! Surfaces: the unit a host window presents.
//!
//! A `Surface` owns a UI tree, a renderer and a damage list, and produces frames
//! on demand. It also owns the registry of [`DirectSurface`] children — the
//! escape hatch that lets another thread write pixels straight into a region of
//! the UI (CloverPic's canvas) without going through the UI thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clover_core::{Damage, Point, Premul, Rect, Size};
use clover_ui::{node::Node, NodeId, RouteEntry, SurfaceHandle, UiEvent, UiTree};

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
    damage: Damage,
    direct: HashMap<SurfaceHandle, Arc<DirectSurface>>,
    generation: u64,
}

impl Surface {
    /// Creates a surface with a system font.
    pub fn new(root: Node, width: f32, height: f32, scale: f32) -> Option<Self> {
        let physical = physical_size(width, height, scale);
        let renderer = CpuRenderer::new(physical.0, physical.1)?;
        let mut surface = Self {
            tree: UiTree::new(root),
            renderer,
            damage: Damage::new(),
            direct: HashMap::new(),
            generation: 0,
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
        self.damage.mark_full();
    }

    /// Registers a direct-memory child surface.
    pub fn register_direct(&mut self, handle: SurfaceHandle, surface: Arc<DirectSurface>) {
        self.direct.insert(handle, surface);
        self.damage.mark_full();
    }

    pub fn direct(&self, handle: SurfaceHandle) -> Option<&Arc<DirectSurface>> {
        self.direct.get(&handle)
    }

    /// Marks the whole surface dirty.
    pub fn invalidate_all(&mut self) {
        self.damage.mark_full();
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
        if self.damage.is_empty() {
            return;
        }
        let metrics = &self.renderer;
        self.tree.layout(metrics);

        let list = build(&self.tree, self.tree.scale(), &self.renderer.text, &self.renderer.images);
        let bounds = Rect::new(0.0, 0.0, list.width as f32, list.height as f32);
        let damage = self.damage.take(bounds);
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

    /// Routes an input event and returns the node chain, capture-first.
    pub fn dispatch(&mut self, event: UiEvent) -> Vec<RouteEntry> {
        match event {
            UiEvent::Pointer(p) => {
                self.tree.update_hover(Some(p.position));
                if p.action == clover_ui::PointerAction::Down {
                    // Focus the innermost focusable ancestor of the hit node.
                    let chain = self.tree.hit_chain(p.position);
                    let focus_target = chain
                        .iter()
                        .copied()
                        .find(|id| self.tree.node(*id).map(|n| n.focusable).unwrap_or(false));
                    self.tree.set_focus(focus_target);

                    let id = self.hit_node(p.position);
                    let bounds = self.hit_bounds(p.position);
                    if let Some(node) = self.tree.node_mut(id) {
                        node.state.pressed = true;
                    }
                    self.damage.add(bounds);
                }
            }
            UiEvent::Wheel(_) => {}
            UiEvent::Key(k) => {
                // Tab / Shift+Tab move keyboard focus before routing.
                if k.action == clover_ui::KeyAction::Down && k.key == 0x09 && !k.repeat {
                    self.tree.focus_next(k.modifiers.shift());
                }
            }
        }
        clover_ui::event::route(&self.tree, &event)
    }

    fn hit_node(&self, point: Point) -> NodeId {
        self.tree
            .hit_chain(point)
            .first()
            .copied()
            .unwrap_or(NodeId(0))
    }

    fn hit_bounds(&self, point: Point) -> Rect {
        self.tree
            .hit_chain(point)
            .first()
            .and_then(|id| self.tree.node(*id))
            .map(|n| n.bounds)
            .unwrap_or(Rect::ZERO)
    }
}

fn physical_size(width: f32, height: f32, scale: f32) -> (u32, u32) {
    let scale = scale.max(0.01);
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
}
