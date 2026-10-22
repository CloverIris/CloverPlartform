//! Platform-neutral primitives shared by every CloverPlatform crate.
//!
//! This crate deliberately has no dependencies: geometry, colour, damage and a
//! few threading helpers are the vocabulary that the UI, render and ABI layers
//! all speak.

pub mod color;
pub mod damage;
pub mod geometry;
pub mod path;
pub mod thread;

pub use color::{Color, Premul};
pub use damage::Damage;
pub use geometry::{Align, Edges, Orientation, Point, Rect, Size};
pub use path::{PathCmd, PathGeometry};
