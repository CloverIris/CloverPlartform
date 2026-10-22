//! UI events and routing.
//!
//! The framework routes; the application decides semantics. [`route`] produces
//! the capture → target → bubble chain for an event so a retained-mode app can
//! subscribe at any level, exactly like WPF's routed events.

use clover_core::Point;

use crate::node::NodeId;

/// Pointer action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAction {
    Move,
    Down,
    Up,
    Cancel,
}

/// Which button changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseButton {
    #[default]
    None,
    Left,
    Right,
    Middle,
}

/// Modifier keys, mirroring CloverPic's `Input::ModifierKeys` bit layout so the
/// adapter can forward its value unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers(pub u32);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: Modifiers = Modifiers(1 << 0);
    pub const CONTROL: Modifiers = Modifiers(1 << 1);
    pub const ALT: Modifiers = Modifiers(1 << 2);

    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn shift(self) -> bool {
        self.contains(Modifiers::SHIFT)
    }
    pub fn control(self) -> bool {
        self.contains(Modifiers::CONTROL)
    }
    pub fn alt(self) -> bool {
        self.contains(Modifiers::ALT)
    }
}

/// A pointer event in surface logical coordinates.
#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    pub action: PointerAction,
    pub position: Point,
    pub button: MouseButton,
    pub modifiers: Modifiers,
    /// Distinguishes concurrent pointers (mouse, pen, touch).
    pub pointer_id: u64,
    /// `1.0` for a mouse; pens report their pressure.
    pub pressure: f32,
    /// `Some` when the platform reports a double click.
    pub click_count: u8,
}

impl Default for PointerEvent {
    fn default() -> Self {
        Self {
            action: PointerAction::Move,
            position: Point::ZERO,
            button: MouseButton::None,
            modifiers: Modifiers::NONE,
            pointer_id: 0,
            pressure: 1.0,
            click_count: 1,
        }
    }
}

/// Key action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Down,
    Up,
}

/// A key event, using the same virtual-key numbering CloverPic core uses.
#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub action: KeyAction,
    pub key: u32,
    pub modifiers: Modifiers,
    pub repeat: bool,
}

/// A wheel or trackpad scroll, `delta` in notches (positive = up).
#[derive(Clone, Copy, Debug)]
pub struct WheelEvent {
    pub delta: f32,
    pub position: Point,
    pub modifiers: Modifiers,
}

/// Any event the UI layer understands.
#[derive(Clone, Copy, Debug)]
pub enum UiEvent {
    Pointer(PointerEvent),
    Key(KeyEvent),
    Wheel(WheelEvent),
}

/// Routing phase of a single hop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventPhase {
    Capture,
    Target,
    Bubble,
}

/// One hop of a routed event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    pub node: NodeId,
    pub phase: EventPhase,
}

/// Computes the routed chain for `event`.
///
/// Pointer and wheel events target the deepest hit node; key events target the
/// focused node. The chain is ordered capture (root → target), target, then
/// bubble (target → root).
pub fn route(tree: &crate::node::UiTree, event: &UiEvent) -> Vec<RouteEntry> {
    let chain: Vec<NodeId> = match event {
        UiEvent::Pointer(p) => tree.hit_chain(p.position),
        UiEvent::Wheel(w) => tree.hit_chain(w.position),
        // Key events follow keyboard focus, not the pointer.
        UiEvent::Key(_) => match tree.focus() {
            Some(id) => ancestor_chain(tree, id),
            None => Vec::new(),
        },
    };
    from_chain(&chain)
}

/// Builds the routed chain from a deepest-first ancestor list.
pub fn from_chain(deepest_first: &[NodeId]) -> Vec<RouteEntry> {
    let mut out = Vec::with_capacity(deepest_first.len() * 2);
    let Some(target) = deepest_first.first().copied() else {
        return out;
    };
    // Capture runs root -> target's parent, then the target, then bubble runs
    // target's parent -> root.
    for id in deepest_first.iter().skip(1).rev() {
        out.push(RouteEntry { node: *id, phase: EventPhase::Capture });
    }
    out.push(RouteEntry { node: target, phase: EventPhase::Target });
    for id in deepest_first.iter().skip(1) {
        out.push(RouteEntry { node: *id, phase: EventPhase::Bubble });
    }
    out
}

fn ancestor_chain(tree: &crate::node::UiTree, id: NodeId) -> Vec<NodeId> {
    let mut chain = Vec::new();
    if tree.node(id).is_none() {
        return chain;
    }
    chain.push(id);
    // Walk from the root down, collecting ancestors of `id`.
    fn visit(node: &crate::node::Node, target: NodeId, stack: &mut Vec<NodeId>) -> bool {
        stack.push(node.id);
        if node.id == target {
            return true;
        }
        for child in &node.children {
            if visit(child, target, stack) {
                return true;
            }
        }
        stack.pop();
        false
    }
    let mut stack = Vec::new();
    visit(tree.root(), id, &mut stack);
    // Deepest first.
    stack.reverse();
    stack
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::UiTree;
    use crate::style::Length;
    use crate::{layout::EmptyMetrics, node};
    use clover_core::Size;

    fn sample_tree() -> UiTree {
        let mut tree = UiTree::new(
            node::column()
                .name("root")
                .padding(clover_core::Edges::all(10.0))
                .child(node::panel().size(Length::Px(50.0), Length::Px(50.0)).name("inner")),
        );
        tree.resize(Size::new(100.0, 100.0), 1.0);
        tree.layout(&EmptyMetrics);
        tree
    }

    #[test]
    fn pointer_route_bubbles_to_root() {
        let tree = sample_tree();
        let event = UiEvent::Pointer(PointerEvent { position: Point::new(20.0, 20.0), ..Default::default() });
        let route = route(&tree, &event);
        assert_eq!(route.first().unwrap().phase, EventPhase::Capture);
        assert!(route.iter().any(|e| e.phase == EventPhase::Target));
        assert_eq!(route.last().unwrap().phase, EventPhase::Bubble);
        // Root is captured first and bubbled last.
        assert_eq!(route.first().unwrap().node, route.last().unwrap().node);
    }

    #[test]
    fn target_is_deepest_hit_node() {
        let tree = sample_tree();
        let event = UiEvent::Pointer(PointerEvent { position: Point::new(20.0, 20.0), ..Default::default() });
        let route = route(&tree, &event);
        let target = route.iter().find(|e| e.phase == EventPhase::Target).unwrap();
        let inner = tree.node(target.node).unwrap();
        assert_eq!(inner.name.as_deref(), Some("inner"));
    }

    #[test]
    fn key_events_target_the_focused_node() {
        let mut tree = sample_tree();
        let inner = tree.root().children[0].id;
        tree.set_focus(Some(inner));
        let event = UiEvent::Key(KeyEvent {
            action: KeyAction::Down,
            key: 0x09,
            modifiers: Modifiers::NONE,
            repeat: false,
        });
        let route = route(&tree, &event);
        let target = route.iter().find(|e| e.phase == EventPhase::Target).unwrap();
        assert_eq!(target.node, inner);
        assert_eq!(route.len(), 3, "capture root, target inner, bubble root");
    }

    #[test]
    fn key_events_without_focus_route_nowhere() {
        let tree = sample_tree();
        let event = UiEvent::Key(KeyEvent {
            action: KeyAction::Down,
            key: 0x41,
            modifiers: Modifiers::NONE,
            repeat: false,
        });
        assert!(route(&tree, &event).is_empty());
    }

    #[test]
    fn focus_cycles_through_focusable_nodes() {
        let mut tree = crate::node::UiTree::new(
            node::row()
                .child(node::panel().name("a").focusable(true))
                .child(node::panel().name("b"))
                .child(node::panel().name("c").focusable(true)),
        );
        tree.resize(Size::new(100.0, 20.0), 1.0);
        tree.layout(&EmptyMetrics);

        assert!(tree.focus_next(false));
        let first = tree.focus().unwrap();
        assert_eq!(tree.node(first).unwrap().name.as_deref(), Some("a"));
        assert!(tree.node(first).unwrap().state.focused);

        tree.focus_next(false);
        let second = tree.focus().unwrap();
        assert_eq!(tree.node(second).unwrap().name.as_deref(), Some("c"));

        tree.focus_next(false);
        assert_eq!(tree.focus().unwrap(), first, "wraps to the start");

        tree.focus_next(true);
        assert_eq!(tree.focus().unwrap(), second, "reverse wraps backwards");
    }

    #[test]
    fn clearing_focus_updates_state() {
        let mut tree = sample_tree();
        let inner = tree.root().children[0].id;
        tree.set_focus(Some(inner));
        assert!(tree.node(inner).unwrap().state.focused);
        tree.set_focus(None);
        assert!(tree.focus().is_none());
        assert!(!tree.node(inner).unwrap().state.focused);
    }

    #[test]
    fn modifiers_are_bitflags() {
        let m = Modifiers(Modifiers::SHIFT.0 | Modifiers::CONTROL.0);
        assert!(m.shift() && m.control() && !m.alt());
    }

    #[test]
    fn empty_chain_routes_to_nothing() {
        assert!(from_chain(&[]).is_empty());
    }

    #[test]
    fn chain_order_is_capture_target_bubble() {
        let chain = [NodeId(3), NodeId(2), NodeId(1)];
        let route = from_chain(&chain);
        let nodes: Vec<(u64, EventPhase)> = route.iter().map(|e| (e.node.0, e.phase)).collect();
        assert_eq!(
            nodes,
            vec![
                (1, EventPhase::Capture),
                (2, EventPhase::Capture),
                (3, EventPhase::Target),
                (2, EventPhase::Bubble),
                (1, EventPhase::Bubble),
            ]
        );
    }
}
