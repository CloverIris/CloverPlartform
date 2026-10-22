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

pub mod event;
pub mod layout;
pub mod node;
pub mod style;
pub mod xaml;

pub use event::{EventPhase, MouseButton, PointerAction, PointerEvent, RouteEntry, UiEvent};
pub use layout::{EmptyMetrics, LayoutMetrics};
pub use node::{ImageHandle, Node, NodeId, NodeMode, SurfaceHandle, UiTree, View};
pub use style::{Fit, Length, NodeState, Style};
