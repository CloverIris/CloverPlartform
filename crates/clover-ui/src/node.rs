//! The node tree and its two authoring modes.
//!
//! There is exactly **one** tree representation. XAML, the declarative builders
//! and direct retained-mode mutation all produce the same [`Node`], which is what
//! makes mixing them inside a single layout possible:
//!
//! * [`NodeMode::Retained`] — the host owns this node and mutates it in place
//!   through its id (what CloverPic's editor chrome wants).
//! * [`NodeMode::Declarative`] — the node's children are regenerated from a
//!   builder whenever the mount point is invalidated (what Compose/Flutter style
//!   code wants).
//!
//! The mode is per-node, not per-tree, so a declarative toolbar can sit inside a
//! retained shell and a retained canvas can sit inside a declarative page.

use clover_core::{Align, Color, Edges, Orientation, Point, Rect, Size};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::style::{Fit, Length, NodeState, Style};

static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1);

fn next_node_id() -> u64 {
    NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Stable identity of a node inside one tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// Handle into the tree's image registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageHandle(pub u64);

/// Handle into the tree's direct-memory surface registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceHandle(pub u64);

/// How a node's children are produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeMode {
    /// Host-owned, mutated in place by id.
    #[default]
    Retained,
    /// Rebuilt from a builder registered on the tree's mount point.
    Declarative,
}

/// What a node draws.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum View {
    /// Container or plain background rectangle.
    #[default]
    Empty,
    /// Single-line text, using the node's font size and text colour.
    Text(String),
    /// A decoded image fitted into the node's content box.
    Image { handle: ImageHandle, fit: Fit },
    /// A child surface whose pixels are written by another thread.
    Surface { handle: SurfaceHandle },
    /// An editable single-line text field.
    TextInput {
        /// Current contents.
        text: String,
        /// Shown when `text` is empty.
        placeholder: String,
        /// Byte index of the caret inside `text`.
        caret: usize,
        /// Text an input method is currently composing, not yet committed.
        composition: Option<Composition>,
    },
}

/// Text an input method is composing.
///
/// The composition is *preview* text: it is displayed inline at the caret with an
/// underline, but it is not part of `text` until committed. That is what lets an
/// IME replace, cancel or confirm a phrase without the host ever seeing partial
/// keystrokes.
#[derive(Clone, Debug, PartialEq)]
pub struct Composition {
    pub text: String,
    /// Caret inside `text`, in bytes.
    pub cursor: usize,
}

/// Builds what a text field should display, plus the byte range of any
/// composition inside it.
pub fn text_input_display(
    text: &str,
    composition: Option<&Composition>,
    caret: usize,
) -> (String, Option<(usize, usize)>) {
    let caret = caret.min(text.len());
    match composition {
        Some(composition) if !composition.text.is_empty() => {
            let mut displayed = String::with_capacity(text.len() + composition.text.len());
            displayed.push_str(&text[..caret]);
            let start = displayed.len();
            displayed.push_str(&composition.text);
            let end = displayed.len();
            displayed.push_str(&text[caret..]);
            (displayed, Some((start, end)))
        }
        _ => (text.to_string(), None),
    }
}


/// A running interpolation on a node.
///
/// Only opacity and background colour animate today. The state lives on the node
/// so the display list needs no clock: `Surface::advance` writes the eased value
/// into `Style` and damages the node.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    pub start_ms: u64,
    pub duration_ms: u64,
    pub from_opacity: f32,
    pub to_opacity: f32,
    pub from_background: Color,
    pub to_background: Color,
}

impl Animation {
    /// Eased progress in `0.0..=1.0` at `now_ms`.
    pub fn progress(&self, now_ms: u64) -> f32 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        let elapsed = now_ms.saturating_sub(self.start_ms) as f32;
        let t = (elapsed / self.duration_ms as f32).clamp(0.0, 1.0);
        // Smoothstep: cheap, and avoids the visible jerk of linear easing.
        t * t * (3.0 - 2.0 * t)
    }

    pub fn is_finished(&self, now_ms: u64) -> bool {
        self.duration_ms == 0 || now_ms.saturating_sub(self.start_ms) >= self.duration_ms
    }
}

/// Scroll state for a container whose content may exceed its box.
///
/// Vertical only for now: the offset is clamped by the layout pass once the
/// content height is known.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollState {
    pub enabled: bool,
    /// Current vertical offset in logical pixels, `0..=max_offset`.
    pub offset: f32,
    /// Content height measured by the last layout pass.
    pub content_height: f32,
}

impl ScrollState {
    pub fn max_offset(&self, viewport_height: f32) -> f32 {
        (self.content_height - viewport_height).max(0.0)
    }

    pub fn is_scrollable(&self, viewport_height: f32) -> bool {
        self.enabled && self.max_offset(viewport_height) > 0.5
    }
}

/// One node of the UI tree.
#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    /// Stable identity across rebuilds; lets reconciliation keep state.
    pub key: Option<String>,
    /// `x:Name` from XAML, the handle retained-mode code looks up.
    pub name: Option<String>,
    pub mode: NodeMode,
    pub style: Style,
    pub view: View,
    pub children: Vec<Node>,
    /// Filled in by the layout pass.
    pub bounds: Rect,
    pub state: NodeState,
    pub visible: bool,
    /// Participates in Tab traversal and can receive key events.
    pub focusable: bool,
    /// Vertical scrolling state; only used when `scroll.enabled`.
    pub scroll: ScrollState,
    /// Running animation, applied by `Surface::advance`.
    pub animation: Option<Animation>,
    /// Style classes matched by `Selector::Class`.
    pub classes: Vec<String>,
    /// Command identifier reported to the host when this node is activated.
    ///
    /// The framework decides *when* activation happens; the host decides what the
    /// command means. `None` means the node is not actionable.
    pub action: Option<String>,
    /// Identifies this node as a drag handle.
    ///
    /// Dragging reports `drag:begin:<payload>`, `drag:move:<payload>:<dx>:<dy>`
    /// and `drag:end:<payload>` actions. The framework owns the gesture and the
    /// deltas; the host owns what the payload means.
    pub drag_payload: Option<String>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            id: NodeId(0),
            key: None,
            name: None,
            mode: NodeMode::Retained,
            style: Style::default(),
            view: View::Empty,
            children: Vec::new(),
            bounds: Rect::ZERO,
            state: NodeState::default(),
            visible: true,
            focusable: false,
            scroll: ScrollState::default(),
            animation: None,
            classes: Vec::new(),
            action: None,
            drag_payload: None,
        }
    }
}

impl Node {
    /// A node with a fresh identity.
    pub fn new() -> Self {
        Self { id: NodeId(next_node_id()), ..Self::default() }
    }

    fn with_view(view: View) -> Self {
        Self { view, ..Self::new() }
    }

    // ---- authoring -----------------------------------------------------

    pub fn child(mut self, child: Node) -> Self {
        self.children.push(child);
        self
    }

    pub fn children_of(mut self, children: impl IntoIterator<Item = Node>) -> Self {
        self.children.extend(children);
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn mode(mut self, mode: NodeMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn width(mut self, width: Length) -> Self {
        self.style.width = width;
        self
    }

    pub fn height(mut self, height: Length) -> Self {
        self.style.height = height;
        self
    }

    pub fn size(mut self, w: Length, h: Length) -> Self {
        self.style.width = w;
        self.style.height = h;
        self
    }

    pub fn min_size(mut self, w: Option<f32>, h: Option<f32>) -> Self {
        self.style.min_width = w;
        self.style.min_height = h;
        self
    }

    pub fn max_size(mut self, w: Option<f32>, h: Option<f32>) -> Self {
        self.style.max_width = w;
        self.style.max_height = h;
        self
    }

    pub fn padding(mut self, padding: Edges) -> Self {
        self.style.padding = padding;
        self
    }

    pub fn margin(mut self, margin: Edges) -> Self {
        self.style.margin = margin;
        self
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.style.gap = gap;
        self
    }

    pub fn direction(mut self, direction: Orientation) -> Self {
        self.style.direction = direction;
        self
    }

    /// Places this node absolutely, outside the parent's flow.
    pub fn position(mut self, x: f32, y: f32) -> Self {
        self.style.position = Some(clover_core::Point::new(x, y));
        self
    }

    /// Makes this node a drag source identified by `payload`.
    pub fn drag(mut self, payload: impl Into<String>) -> Self {
        self.drag_payload = Some(payload.into());
        self
    }

    /// Lays children out in `columns` uniform columns.
    pub fn grid_columns(mut self, columns: u32) -> Self {
        self.style.grid_columns = Some(columns.max(1));
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.style.align = align;
        self
    }

    pub fn justify(mut self, justify: Align) -> Self {
        self.style.justify = justify;
        self
    }

    pub fn background(mut self, color: Color) -> Self {
        self.style.background = color;
        self
    }

    /// Fills the background with a gradient.
    pub fn brush(mut self, brush: crate::style::Brush) -> Self {
        self.style.brush = Some(brush);
        self
    }

    /// Uses `handle` as a fitted background texture.
    pub fn background_image(mut self, handle: ImageHandle, fit: crate::style::Fit) -> Self {
        self.style.brush = Some(crate::style::Brush::Texture { handle, fit, opacity: 1.0 });
        self
    }

    pub fn radius(mut self, radius: f32) -> Self {
        self.style.radius = radius;
        self
    }

    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.style.border_width = width;
        self.style.border_color = color;
        self
    }

    pub fn font_size(mut self, size: f32) -> Self {
        self.style.font_size = size;
        self
    }

    pub fn text_color(mut self, color: Color) -> Self {
        self.style.text_color = color;
        self
    }

    pub fn text_align(mut self, align: Align) -> Self {
        self.style.text_align = align;
        self
    }

    pub fn opacity(mut self, opacity: f32) -> Self {
        self.style.opacity = opacity;
        self
    }

    pub fn clip(mut self, clip: bool) -> Self {
        self.style.clip = clip;
        self
    }

    pub fn visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    pub fn focusable(mut self, focusable: bool) -> Self {
        self.focusable = focusable;
        self
    }

    /// Word-wraps text to the node's content width.
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.style.wrap = wrap;
        self
    }

    /// Adds style classes.
    pub fn classes(mut self, classes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.classes.extend(classes.into_iter().map(Into::into));
        self
    }

    /// Makes this node an editable single-line text field.
    pub fn text_input(mut self, text: impl Into<String>, placeholder: impl Into<String>) -> Self {
        let text = text.into();
        let caret = text.len();
        self.view = View::TextInput { text, placeholder: placeholder.into(), caret, composition: None };
        self.focusable = true;
        self
    }

    /// Reports `action` to the host when this node is activated by a click or a
    /// key press.
    pub fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// Makes this container vertically scrollable and clipping.
    pub fn scrollable(mut self) -> Self {
        self.scroll.enabled = true;
        self.style.clip = true;
        self.style.scroll_viewport = true;
        self
    }

    // ---- queries -------------------------------------------------------

    /// Depth-first search by id.
    pub fn find(&self, id: NodeId) -> Option<&Node> {
        if self.id == id {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find(id) {
                return Some(found);
            }
        }
        None
    }

    /// Depth-first search by id.
    pub fn find_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        if self.id == id {
            return Some(self);
        }
        for child in &mut self.children {
            if let Some(found) = child.find_mut(id) {
                return Some(found);
            }
        }
        None
    }

    /// Depth-first search by `x:Name`.
    pub fn find_named(&self, name: &str) -> Option<&Node> {
        if self.name.as_deref() == Some(name) {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find_named(name) {
                return Some(found);
            }
        }
        None
    }

    pub fn find_named_mut(&mut self, name: &str) -> Option<&mut Node> {
        if self.name.as_deref() == Some(name) {
            return Some(self);
        }
        for child in &mut self.children {
            if let Some(found) = child.find_named_mut(name) {
                return Some(found);
            }
        }
        None
    }

    /// Visits every node depth-first, parents before children.
    pub fn walk(&self, visit: &mut dyn FnMut(&Node)) {
        visit(self);
        for child in &self.children {
            child.walk(visit);
        }
    }

    /// Nearest ancestor of `id` (inclusive) that is a drag handle.
    pub fn find_drag_ancestor(&self, id: NodeId) -> Option<(NodeId, &str)> {
        fn walk<'a>(
            node: &'a Node,
            target: NodeId,
            current: Option<(NodeId, &'a str)>,
        ) -> Option<Option<(NodeId, &'a str)>> {
            let current = match node.drag_payload.as_deref() {
                Some(payload) => Some((node.id, payload)),
                None => current,
            };
            if node.id == target {
                return Some(current);
            }
            for child in &node.children {
                if let Some(found) = walk(child, target, current) {
                    return Some(found);
                }
            }
            None
        }
        walk(self, id, None).flatten()
    }

    /// Nearest ancestor of `id` (inclusive) that carries an action.
    pub fn find_action_ancestor(&self, id: NodeId) -> Option<(NodeId, &str)> {
        fn walk<'a>(node: &'a Node, target: NodeId, current: Option<(NodeId, &'a str)>) -> Option<Option<(NodeId, &'a str)>> {
            let current = match node.action.as_deref() {
                Some(action) => Some((node.id, action)),
                None => current,
            };
            if node.id == target {
                return Some(current);
            }
            for child in &node.children {
                if let Some(found) = walk(child, target, current) {
                    return Some(found);
                }
            }
            None
        }
        walk(self, id, None).flatten()
    }

    /// Nearest scrollable ancestor of `id`, including `id` itself.
    pub fn find_scrollable_ancestor(&self, id: NodeId) -> Option<NodeId> {
        fn walk(node: &Node, target: NodeId, current: Option<NodeId>) -> Option<Option<NodeId>> {
            let current = if node.scroll.enabled { Some(node.id) } else { current };
            if node.id == target {
                return Some(current);
            }
            for child in &node.children {
                if let Some(found) = walk(child, target, current) {
                    return Some(found);
                }
            }
            None
        }
        walk(self, id, None).flatten()
    }

    /// Collects focusable, visible nodes in depth-first (tab) order.
    pub fn collect_focusables(&self, out: &mut Vec<NodeId>) {
        if !self.visible {
            return;
        }
        if self.focusable {
            out.push(self.id);
        }
        for child in &self.children {
            child.collect_focusables(out);
        }
    }

    /// Counts nodes in the subtree, including `self`.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Node::count).sum::<usize>()
    }

    /// Assigns fresh identities to the whole subtree.
    ///
    /// Used when a declarative builder hands back a tree that was not produced by
    /// reconciliation, so ids never collide with live nodes.
    pub fn reidentify(&mut self) {
        self.id = NodeId(next_node_id());
        for child in &mut self.children {
            child.reidentify();
        }
    }

    /// Ancestor chain containing `point`, ordered deepest first.
    pub fn hit_chain(&self, point: Point) -> Vec<NodeId> {
        let mut chain = Vec::new();
        self.hit_chain_into(point, &mut chain);
        // `hit_chain_into` pushes the hit node before its ancestors, so the
        // vector already runs deepest-first.
        chain
    }

    fn hit_chain_into(&self, point: Point, out: &mut Vec<NodeId>) -> bool {
        if !self.visible || !self.bounds.contains(point) {
            return false;
        }
        // Later children paint on top, so test them first.
        for child in self.children.iter().rev() {
            if child.hit_chain_into(point, out) {
                out.push(self.id);
                return true;
            }
        }
        out.push(self.id);
        true
    }

    /// Walks the subtree and marks `hovered` for ids in `hovered`.
    pub fn apply_hover(&mut self, hovered: Option<NodeId>) {
        self.state.hovered = hovered == Some(self.id);
        for child in &mut self.children {
            child.apply_hover(hovered);
        }
    }

    /// Collects the bounds of every node whose state changed, for damage.
    pub fn collect_changed_bounds(&self, out: &mut Vec<Rect>) {
        let _ = out;
    }
}

// ---- declarative constructors -----------------------------------------

/// A vertical stack container.
pub fn column() -> Node {
    Node::new().direction(Orientation::Vertical)
}

/// A horizontal stack container.
pub fn row() -> Node {
    Node::new().direction(Orientation::Horizontal)
}

/// A stack with an explicit direction.
pub fn stack(direction: Orientation) -> Node {
    Node::new().direction(direction)
}

/// A uniform grid container: `columns` equal cells per row.
pub fn grid(columns: u32) -> Node {
    let mut node = Node::new();
    node.style.grid_columns = Some(columns.max(1));
    node.style.direction = Orientation::Vertical;
    node
}

/// A plain panel, useful as a positioned box.
pub fn panel() -> Node {
    Node::new()
}

/// Single-line text.
pub fn text(content: impl Into<String>) -> Node {
    Node::with_view(View::Text(content.into()))
}

/// A button-looking container holding a label.
pub fn button(label: impl Into<String>) -> Node {
    let label = label.into();
    let mut node = Node::with_view(View::Text(label.clone()))
        .padding(Edges::symmetric(14.0, 8.0))
        .radius(6.0)
        .background(Color::rgb(0x38, 0x3E, 0x46))
        .border(1.0, Color::rgb(0x60, 0x68, 0x74))
        .text_align(Align::Center);
    node.key = Some(format!("button:{label}"));
    node
}

/// An image drawn from the tree's image registry.
pub fn image(handle: ImageHandle) -> Node {
    Node::with_view(View::Image { handle, fit: Fit::Contain })
}

/// An editable single-line text field.
pub fn text_input(text: impl Into<String>, placeholder: impl Into<String>) -> Node {
    Node::new().text_input(text, placeholder)
}

/// A child surface whose pixels another thread writes directly.
pub fn direct_surface(handle: SurfaceHandle) -> Node {
    Node::with_view(View::Surface { handle })
}

// ---- tree --------------------------------------------------------------

/// Builder registered for a declarative mount point.
type MountBuilder = Box<dyn FnMut() -> Node + Send>;

/// The UI tree: root node, name index, mount points and dirty bookkeeping.
pub struct UiTree {
    root: Node,
    viewport: Size,
    scale: f32,
    mounts: HashMap<NodeId, MountBuilder>,
    invalidated: Vec<NodeId>,
    damage: clover_core::Damage,
    focus: Option<NodeId>,
    pressed: Option<NodeId>,
    /// Set when geometry must be recomputed; cleared after the layout pass.
    layout_dirty: bool,
}

impl UiTree {
    pub fn new(root: Node) -> Self {
        Self {
            root,
            viewport: Size::ZERO,
            scale: 1.0,
            mounts: HashMap::new(),
            invalidated: Vec::new(),
            damage: clover_core::Damage::new(),
            focus: None,
            pressed: None,
            layout_dirty: true,
        }
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Node {
        &mut self.root
    }

    /// Replaces the whole tree.
    ///
    /// Focus, press and mount state all refer to the old node ids, so they are
    /// cleared. The host is expected to rebuild the tree when its data changes.
    pub fn set_root(&mut self, root: Node) {
        self.root = root;
        self.focus = None;
        self.pressed = None;
        self.mounts.clear();
        self.invalidated.clear();
        self.layout_dirty = true;
        self.damage.mark_full();
    }

    pub fn viewport(&self) -> Size {
        self.viewport
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn damage(&mut self) -> &mut clover_core::Damage {
        &mut self.damage
    }

    /// Takes the pending damage, clipped to `bounds` and snapped to whole pixels.
    pub fn take_damage(&mut self, bounds: Rect) -> Vec<Rect> {
        self.damage.take(bounds)
    }

    /// Whether anything is waiting to be repainted.
    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }

    /// Resizes the surface; everything becomes dirty.
    pub fn resize(&mut self, viewport: Size, scale: f32) {
        if self.viewport == viewport && (self.scale - scale).abs() < f32::EPSILON {
            return;
        }
        self.viewport = viewport;
        self.scale = scale.max(0.01);
        self.layout_dirty = true;
        self.damage.mark_full();
    }

    /// Finds a node by id.
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.root.find(id)
    }

    /// Finds a node by id, mutably.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.root.find_mut(id)
    }

    /// Finds a node by `x:Name`.
    pub fn named(&self, name: &str) -> Option<&Node> {
        self.root.find_named(name)
    }

    /// Finds a node by `x:Name`, mutably. The caller is responsible for
    /// invalidating the affected region.
    pub fn named_mut(&mut self, name: &str) -> Option<&mut Node> {
        self.root.find_named_mut(name)
    }

    /// Registers a declarative subtree.
    ///
    /// The mount point is a normal node that hosts whatever `builder` returns, so
    /// retained siblings and declarative subtrees can be mixed freely. Returns
    /// the mount point's id.
    pub fn mount(&mut self, parent: NodeId, key: &str, mut builder: MountBuilder) -> NodeId {
        let mount_id = NodeId(next_node_id());
        let built = builder();
        let mut mount_node = Node::new();
        mount_node.id = mount_id;
        mount_node.key = Some(format!("mount:{key}"));
        mount_node.mode = NodeMode::Declarative;
        mount_node.style.clip = true;
        mount_node.children = built.children;

        let parent_node = self.root.find_mut(parent);
        match parent_node {
            Some(node) => node.children.push(mount_node),
            None => self.root.children.push(mount_node),
        }
        self.mounts.insert(mount_id, builder);
        self.layout_dirty = true;
        self.damage.mark_full();
        mount_id
    }

    /// Schedules a declarative subtree for rebuild on the next [`Self::flush`].
    pub fn invalidate_mount(&mut self, mount: NodeId) {
        self.invalidated.push(mount);
    }

    /// Rebuilds every invalidated declarative subtree, reusing ids by `key`.
    ///
    /// Returns `true` when anything actually changed.
    pub fn flush(&mut self) -> bool {
        if self.invalidated.is_empty() {
            return false;
        }
        let pending: Vec<NodeId> = self.invalidated.drain(..).collect();
        let mut changed = false;
        for mount_id in pending {
            let Some(builder) = self.mounts.get_mut(&mount_id) else { continue };
            let built = builder();
            let Some(node) = self.root.find_mut(mount_id) else { continue };
            let old_children = std::mem::take(&mut node.children);
            node.children = reconcile(old_children, built.children);
            changed = true;
        }
        if changed {
            self.layout_dirty = true;
            self.damage.mark_full();
        }
        changed
    }

    /// The node that currently owns keyboard input.
    pub fn focus(&self) -> Option<NodeId> {
        self.focus
    }

    /// Moves focus to `id` (or clears it). Returns whether anything changed.
    pub fn set_focus(&mut self, id: Option<NodeId>) -> bool {
        if self.focus == id {
            return false;
        }
        let mut damage_old = Rect::ZERO;
        if let Some(previous) = self.focus {
            if let Some(node) = self.root.find_mut(previous) {
                node.state.focused = false;
                damage_old = node.bounds;
            }
        }
        self.focus = None;
        if !damage_old.is_empty() {
            self.damage.add(damage_old);
        }

        if let Some(next) = id {
            let mut damage_new = Rect::ZERO;
            let found = if let Some(node) = self.root.find_mut(next) {
                node.state.focused = true;
                damage_new = node.bounds;
                true
            } else {
                false
            };
            if !damage_new.is_empty() {
                self.damage.add(damage_new);
            }
            if found {
                self.focus = Some(next);
            }
        }
        true
    }

    /// The node currently held down by a pointer, if any.
    pub fn pressed(&self) -> Option<NodeId> {
        self.pressed
    }

    /// Sets or clears the pressed node, damaging the affected bounds.
    pub fn set_pressed(&mut self, id: Option<NodeId>) -> bool {
        if self.pressed == id {
            return false;
        }
        let mut changed = false;
        if let Some(previous) = self.pressed {
            let mut bounds = Rect::ZERO;
            if let Some(node) = self.root.find_mut(previous) {
                node.state.pressed = false;
                bounds = node.bounds;
            }
            if !bounds.is_empty() {
                self.damage.add(bounds);
            }
            changed = true;
        }
        self.pressed = None;
        if let Some(next) = id {
            let mut bounds = Rect::ZERO;
            let mut found = false;
            if let Some(node) = self.root.find_mut(next) {
                node.state.pressed = true;
                bounds = node.bounds;
                found = true;
            }
            if !bounds.is_empty() {
                self.damage.add(bounds);
            }
            if found {
                self.pressed = Some(next);
            }
        }
        changed
    }

    /// Moves focus to the next (or previous) focusable node, wrapping around.
    pub fn focus_next(&mut self, reverse: bool) -> bool {
        let mut order = Vec::new();
        self.root.collect_focusables(&mut order);
        if order.is_empty() {
            return self.set_focus(None);
        }
        let current = self.focus.and_then(|id| order.iter().position(|candidate| *candidate == id));
        let index = match current {
            None if reverse => order.len() - 1,
            None => 0,
            Some(index) if reverse => (index + order.len() - 1) % order.len(),
            Some(index) => (index + 1) % order.len(),
        };
        self.set_focus(Some(order[index]))
    }

    /// Runs the layout pass for the current viewport.
    pub fn layout(&mut self, metrics: &dyn crate::layout::LayoutMetrics) {
        if !self.layout_dirty {
            return;
        }
        crate::layout::layout(&mut self.root, self.viewport, metrics);
        self.layout_dirty = false;
        // Geometry moved, so everything on screen may have moved with it.
        self.damage.mark_full();
    }

    /// A structural change happened: geometry must be recomputed.
    ///
    /// Re-running layout every frame looks harmless but is not: the pass marks
    /// the whole root dirty, so damage tracking is defeated and every frame
    /// becomes a full-surface repaint. That is the most expensive mistake this
    /// file has made.
    /// Nearest ancestor of `id` (inclusive) that is a drag handle.
    pub fn find_drag_ancestor(&self, id: NodeId) -> Option<(NodeId, &str)> {
        self.root.find_drag_ancestor(id)
    }

    pub fn invalidate_layout(&mut self) {
        self.layout_dirty = true;
    }

    /// Whether a layout pass is pending.
    pub fn needs_layout(&self) -> bool {
        self.layout_dirty
    }

    /// Hit-tests `point` and returns the node chain from deepest to root.
    pub fn hit_chain(&self, point: Point) -> Vec<NodeId> {
        self.root.hit_chain(point)
    }

    /// Updates hover state, damaging the nodes whose state changed.
    pub fn update_hover(&mut self, point: Option<Point>) {
        let hovered = point.and_then(|p| self.hit_chain(p).first().copied());
        let mut changed: Vec<Rect> = Vec::new();
        apply_hover_recursive(&mut self.root, hovered, &mut changed);
        for rect in changed {
            self.damage.add(rect);
        }
    }
}

fn apply_hover_recursive(node: &mut Node, hovered: Option<NodeId>, changed: &mut Vec<Rect>) {
    let now = hovered == Some(node.id);
    if now != node.state.hovered {
        node.state.hovered = now;
        changed.push(node.bounds);
    }
    for child in &mut node.children {
        apply_hover_recursive(child, hovered, changed);
    }
}

/// Replaces `old` children with `fresh`, keeping ids where keys match.
fn reconcile(old: Vec<Node>, fresh: Vec<Node>) -> Vec<Node> {
    use std::collections::HashMap;
    let mut by_key: HashMap<String, Node> =
        old.into_iter().filter_map(|n| n.key.clone().map(|k| (k, n))).collect();

    fresh
        .into_iter()
        .map(|mut new_node| {
            if let Some(key) = new_node.key.clone() {
                if let Some(mut existing) = by_key.remove(&key) {
                    new_node.id = existing.id;
                    new_node.state = existing.state;
                    let old_children = std::mem::take(&mut existing.children);
                    new_node.children = reconcile(old_children, new_node.children);
                }
            }
            new_node
        })
        .collect()
}
