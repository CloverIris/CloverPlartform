//! CloverPlatform UI layer.
//!
//! One tree, two authoring modes:
//!
//! ```ignore
//! use clover_ui::{node, xaml};
//!
//! // Declarative / Compose-flavoured
//! let page = node::column()
//!     .padding(Edges::all(16.0))
//!     .gap(8.0)
//!     .child(node::text("Hello").font_size(20.0))
//!     .child(node::button("OK"));
//!
//! // XAML / WPF-flavoured
//! let page = xaml::parse(r#"<StackPanel Padding="16" Spacing="8">
//!     <TextBlock Text="Hello" FontSize="20"/>
//!     <Button Content="OK"/>
//! </StackPanel>"#, &xaml::NoImages)?;
//! ```
//!
//! Both produce the same [`node::Node`], so they can be nested in either
//! direction, and a retained shell can host declarative regions through
//! [`node::UiTree::mount`].

pub mod binding;
pub mod event;
pub mod layout;
pub mod node;
pub mod style;
pub mod style_sheet;
pub mod xaml;

pub use event::{
    EventPhase, KeyAction, KeyEvent, Modifiers, MouseButton, PointerAction, PointerEvent,
    RouteEntry, UiEvent, WheelEvent,
};
pub use layout::{EmptyMetrics, LayoutMetrics};
pub use xaml::UiDocument;
pub use node::{ImageHandle, Node, NodeId, NodeMode, ScrollState, SurfaceHandle, UiTree, View};
pub use style::{Fit, Length, NodeState, Style};
pub use binding::{binding_key, DataContext, Value};
pub use style_sheet::{Selector, StyleProps, StyleRule, StyleSheet};
