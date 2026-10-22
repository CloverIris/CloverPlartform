//! C ABI for embedding CloverPlatform in a native host (C++, Objective-C++, C).
//!
//! The host owns the window and the event loop; this layer owns the UI tree, the
//! layout, the rasteriser and the frame buffer. Every handle is opaque.
//!
//! Threading: one `CloverSurface` must be driven from a single thread. Regions of
//! a surface (see `clover_direct_*`) may be written from any thread — that is the
//! point of the direct-memory escape hatch, and it is how a paint canvas keeps
//! its own high-refresh-rate loop.

use std::ffi::{c_char, CStr};
use std::sync::Arc;

use clover_render::{DirectSurface, Surface};
use clover_ui::{node, xaml, Node, SurfaceHandle};

/// Result codes shared by every entry point.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloverResult {
    Ok = 0,
    NullArgument = 1,
    ParseError = 2,
    NoFont = 3,
    UnknownHandle = 4,
}

/// A presented frame. `pixels` is premultiplied BGRA8, valid until the next call
/// that renders this surface.
#[repr(C)]
pub struct CloverFrame {
    pub pixels: *const u32,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub premultiplied: bool,
    pub generation: u64,
}

/// Pointer actions, matching `clover_ui::PointerAction`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum CloverPointerAction {
    Move = 0,
    Down = 1,
    Up = 2,
    Cancel = 3,
}

/// Key actions, matching `clover_ui::KeyAction`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum CloverKeyAction {
    Down = 0,
    Up = 1,
}

/// An opaque surface handle.
pub struct CloverSurface {
    surface: Surface,
    direct: Vec<(SurfaceHandle, Arc<DirectSurface>)>,
    /// Buffers currently locked by `clover_direct_begin_write`.
    pending: Vec<(u64, Vec<u32>)>,
}

/// Creates a surface whose root is parsed from a XAML document.
///
/// # Safety
/// `xaml_utf8` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_create_from_xaml(
    width: f32,
    height: f32,
    scale: f32,
    xaml_utf8: *const c_char,
) -> *mut CloverSurface {
    if xaml_utf8.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(source) = CStr::from_ptr(xaml_utf8).to_str() else {
        return std::ptr::null_mut();
    };
    let Ok(root) = xaml::parse(source, &xaml::NoImages) else {
        return std::ptr::null_mut();
    };
    create(root, width, height, scale)
}

/// Creates a surface from a tree built in Rust and handed over.
#[no_mangle]
pub extern "C" fn clover_surface_create_empty(width: f32, height: f32, scale: f32) -> *mut CloverSurface {
    create(node::panel(), width, height, scale)
}

fn create(root: Node, width: f32, height: f32, scale: f32) -> *mut CloverSurface {
    match Surface::new(root, width, height, scale) {
        Some(surface) => Box::into_raw(Box::new(CloverSurface { surface, direct: Vec::new(), pending: Vec::new() })),
        None => std::ptr::null_mut(),
    }
}

/// Destroys a surface. Null is ignored.
///
/// # Safety
/// `surface` must come from one of the create functions and be destroyed once.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_destroy(surface: *mut CloverSurface) {
    if surface.is_null() {
        return;
    }
    drop(Box::from_raw(surface));
}

/// Resizes the surface and marks everything dirty.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_resize(surface: *mut CloverSurface, width: f32, height: f32, scale: f32) -> CloverResult {
    let Some(surface) = surface.as_mut() else {
        return CloverResult::NullArgument;
    };
    surface.surface.resize(width, height, scale);
    CloverResult::Ok
}

/// Renders any dirty regions and returns the frame.
///
/// # Safety
/// `surface` must be a live handle and `out` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_frame(surface: *mut CloverSurface, out: *mut CloverFrame) -> CloverResult {
    let (Some(surface), false) = (surface.as_mut(), out.is_null()) else {
        return CloverResult::NullArgument;
    };
    let frame = surface.surface.frame();
    *out = CloverFrame {
        pixels: frame.pixels,
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
        premultiplied: frame.premultiplied,
        generation: frame.generation,
    };
    CloverResult::Ok
}

/// Marks the whole surface dirty.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_invalidate(surface: *mut CloverSurface) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    surface.surface.invalidate_all();
    CloverResult::Ok
}

/// Pushes a pointer event. Returns the hit node id, or 0 when nothing was hit.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_pointer(
    surface: *mut CloverSurface,
    x: f32,
    y: f32,
    action: CloverPointerAction,
    button: u32,
    modifiers: u32,
) -> u64 {
    let Some(surface) = surface.as_mut() else { return 0 };
    let action = match action {
        CloverPointerAction::Move => clover_ui::PointerAction::Move,
        CloverPointerAction::Down => clover_ui::PointerAction::Down,
        CloverPointerAction::Up => clover_ui::PointerAction::Up,
        CloverPointerAction::Cancel => clover_ui::PointerAction::Cancel,
    };
    let event = clover_ui::PointerEvent {
        action,
        position: clover_core::Point::new(x, y),
        button: match button {
            1 => clover_ui::MouseButton::Left,
            2 => clover_ui::MouseButton::Right,
            3 => clover_ui::MouseButton::Middle,
            _ => clover_ui::MouseButton::None,
        },
        modifiers: clover_ui::event::Modifiers(modifiers),
        ..Default::default()
    };
    let route = surface.surface.dispatch(clover_ui::UiEvent::Pointer(event));
    route
        .iter()
        .find(|entry| entry.phase == clover_ui::EventPhase::Target)
        .map(|entry| entry.node.0)
        .unwrap_or(0)
}

/// Pushes a key event. Tab and Shift+Tab move focus before the event is routed.
///
/// Returns the id of the focused node after routing, or 0 when nothing has focus.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_key(
    surface: *mut CloverSurface,
    key: u32,
    action: CloverKeyAction,
    modifiers: u32,
    repeat: bool,
) -> u64 {
    let Some(surface) = surface.as_mut() else { return 0 };
    let event = clover_ui::KeyEvent {
        action: match action {
            CloverKeyAction::Down => clover_ui::KeyAction::Down,
            CloverKeyAction::Up => clover_ui::KeyAction::Up,
        },
        key,
        modifiers: clover_ui::Modifiers(modifiers),
        repeat,
    };
    surface.surface.dispatch(clover_ui::UiEvent::Key(event));
    surface.surface.tree().focus().map(|id| id.0).unwrap_or(0)
}

/// Returns the id of the focused node, or 0.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_focus(surface: *mut CloverSurface) -> u64 {
    let Some(surface) = surface.as_mut() else { return 0 };
    surface.surface.tree().focus().map(|id| id.0).unwrap_or(0)
}

/// Moves focus to the next (or previous) focusable node and returns its id.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_focus_next(surface: *mut CloverSurface, reverse: bool) -> u64 {
    let Some(surface) = surface.as_mut() else { return 0 };
    surface.surface.tree_mut().focus_next(reverse);
    surface.surface.tree().focus().map(|id| id.0).unwrap_or(0)
}

/// Registers a direct-memory child surface and returns its handle.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_add_direct(surface: *mut CloverSurface, width: u32, height: u32) -> u64 {
    let Some(surface) = surface.as_mut() else { return 0 };
    let handle = surface.direct.len() as u64 + 1;
    let direct = DirectSurface::new(width, height);
    surface.surface.register_direct(SurfaceHandle(handle), Arc::clone(&direct));
    surface.direct.push((SurfaceHandle(handle), direct));
    handle
}

/// Locks the back buffer of a direct surface for writing and returns its pointer.
///
/// The pointer stays valid until `clover_direct_commit`. Call it from the thread
/// that owns the canvas; the UI thread keeps showing the previous frame meanwhile,
/// which is the swap-chain consistency guarantee.
///
/// # Safety
/// `surface` must be a live handle and `out_pixels`/`out_stride` valid pointers.
#[no_mangle]
pub unsafe extern "C" fn clover_direct_begin_write(
    surface: *mut CloverSurface,
    handle: u64,
    out_pixels: *mut *mut u32,
    out_stride: *mut usize,
    out_height: *mut u32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if out_pixels.is_null() || out_stride.is_null() || out_height.is_null() {
        return CloverResult::NullArgument;
    }
    let Some(entry) = surface.direct.iter().find(|(h, _)| h.0 == handle) else {
        return CloverResult::UnknownHandle;
    };
    let buffer = entry.1.take_back();
    let (ptr, len) = (buffer.as_ptr() as *mut u32, buffer.len());
    // Hand ownership back to the ABI object so the pointer stays valid.
    surface.pending.push((handle, buffer));
    *out_pixels = ptr;
    *out_stride = entry.1.width() as usize;
    *out_height = entry.1.height() as u32;
    let _ = len;
    CloverResult::Ok
}

/// Publishes the buffer locked by `clover_direct_begin_write`.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_direct_commit(surface: *mut CloverSurface, handle: u64) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    let Some(index) = surface.pending.iter().position(|(h, _)| *h == handle) else {
        return CloverResult::UnknownHandle;
    };
    let (_, buffer) = surface.pending.swap_remove(index);
    let Some(entry) = surface.direct.iter().find(|(h, _)| h.0 == handle) else {
        return CloverResult::UnknownHandle;
    };
    entry.1.publish_back(buffer);
    surface.surface.invalidate_all();
    CloverResult::Ok
}

/// Registers an image from a PNG file and returns its handle, or 0 on failure.
///
/// # Safety
/// `path_utf8` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_add_image_png(surface: *mut CloverSurface, path_utf8: *const c_char) -> u64 {
    let Some(surface) = surface.as_mut() else { return 0 };
    if path_utf8.is_null() {
        return 0;
    }
    let Ok(path) = CStr::from_ptr(path_utf8).to_str() else { return 0 };
    match surface.surface.renderer_mut().images.load_png(path) {
        Ok(handle) => handle.0,
        Err(_) => 0,
    }
}

/// Statistics for a surface, useful for host-side diagnostics.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CloverStats {
    pub nodes: u32,
    pub tiles_last_frame: u32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

/// Reads surface statistics.
///
/// # Safety
/// `surface` must be a live handle and `out` a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_stats(surface: *mut CloverSurface, out: *mut CloverStats) -> CloverResult {
    let (Some(surface), false) = (surface.as_mut(), out.is_null()) else {
        return CloverResult::NullArgument;
    };
    let (width, height) = {
        let canvas = surface.surface.canvas();
        (canvas.width, canvas.height)
    };
    *out = CloverStats {
        nodes: surface.surface.tree().root().count() as u32,
        tiles_last_frame: surface.surface.renderer_mut().last_tile_count() as u32,
        width,
        height,
        scale: surface.surface.tree().scale(),
    };
    CloverResult::Ok
}
