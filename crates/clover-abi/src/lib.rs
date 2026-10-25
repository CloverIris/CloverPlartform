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

/// Creates a surface from a XAML document and a `<Styles>` document.
///
/// The stylesheet supplies defaults; attributes on the page override it.
///
/// # Safety
/// Both pointers must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_create_from_xaml_with_styles(
    width: f32,
    height: f32,
    scale: f32,
    xaml_utf8: *const c_char,
    styles_utf8: *const c_char,
) -> *mut CloverSurface {
    if xaml_utf8.is_null() || styles_utf8.is_null() {
        return std::ptr::null_mut();
    }
    let (Ok(source), Ok(styles)) = (
        CStr::from_ptr(xaml_utf8).to_str(),
        CStr::from_ptr(styles_utf8).to_str(),
    ) else {
        return std::ptr::null_mut();
    };
    let Ok(sheet) = xaml::parse_stylesheet(styles) else {
        return std::ptr::null_mut();
    };
    let Ok(root) = xaml::parse_with_styles(source, &xaml::NoImages, &sheet) else {
        return std::ptr::null_mut();
    };
    create(root, width, height, scale)
}

/// Replaces a surface's UI tree with a freshly parsed page.
///
/// `styles_utf8` may be null when the page needs no stylesheet. Focus, press
/// state and the action queue are reset, because they refer to node ids that no
/// longer exist.
///
/// # Safety
/// Both pointers must be null or NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_root_xaml(
    surface: *mut CloverSurface,
    xaml_utf8: *const c_char,
    styles_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if xaml_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(source) = CStr::from_ptr(xaml_utf8).to_str() else {
        return CloverResult::ParseError;
    };
    let root = if styles_utf8.is_null() {
        match xaml::parse(source, &xaml::NoImages) {
            Ok(root) => root,
            Err(_) => return CloverResult::ParseError,
        }
    } else {
        let Ok(styles) = CStr::from_ptr(styles_utf8).to_str() else {
            return CloverResult::ParseError;
        };
        let Ok(sheet) = xaml::parse_stylesheet(styles) else {
            return CloverResult::ParseError;
        };
        match xaml::parse_with_styles(source, &xaml::NoImages, &sheet) {
            Ok(root) => root,
            Err(_) => return CloverResult::ParseError,
        }
    };
    surface.surface.set_root(root);
    CloverResult::Ok
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

/// Pushes a wheel event. Scrolling is applied to the nearest scrollable
/// ancestor of the node under `(x, y)`. Positive `delta` is wheel-up.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_wheel(
    surface: *mut CloverSurface,
    x: f32,
    y: f32,
    delta: f32,
    modifiers: u32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    surface.surface.dispatch(clover_ui::UiEvent::Wheel(clover_ui::WheelEvent {
        delta,
        position: clover_core::Point::new(x, y),
        modifiers: clover_ui::Modifiers(modifiers),
    }));
    CloverResult::Ok
}

/// Pops the next action raised by input into `buffer` (NUL-terminated).
///
/// Returns the length written excluding the terminator, `0` when the queue is
/// empty, or `-1` when the buffer is too small. Drain in a loop after pushing
/// input until it returns 0.
///
/// # Safety
/// `buffer` must be writable for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_take_action(
    surface: *mut CloverSurface,
    buffer: *mut c_char,
    capacity: u32,
) -> i32 {
    let Some(surface) = surface.as_mut() else { return -1 };
    if buffer.is_null() || capacity == 0 {
        return -1;
    }
    let Some(action) = surface.surface.take_action() else { return 0 };
    let bytes = action.as_bytes();
    if bytes.len() + 1 > capacity as usize {
        // Put it back: the host should retry with a bigger buffer.
        surface.surface.push_action(action);
        return -1;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, bytes.len());
    *buffer.add(bytes.len()) = 0;
    bytes.len() as i32
}

/// Shows or hides a named node and repaints its area.
///
/// This is the retained-mode mutation hook: the host changes state it owns and
/// the framework re-renders only what moved.
///
/// # Safety
/// `name_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_visible(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    visible: bool,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else {
        return CloverResult::ParseError;
    };
    let tree = surface.surface.tree_mut();
    let bounds = match tree.named_mut(name) {
        Some(node) => {
            node.visible = visible;
            node.bounds
        }
        None => return CloverResult::UnknownHandle,
    };
    // Visibility changes what the layout pass consumes, so geometry must rerun.
    tree.invalidate_layout();
    tree.damage().add(bounds);
    CloverResult::Ok
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

/// Resizes a direct-memory region. Call after the layout gives the canvas a new
/// box; the contents are cleared.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_direct_resize(
    surface: *mut CloverSurface,
    handle: u64,
    width: u32,
    height: u32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    let Some(entry) = surface.direct.iter().find(|(h, _)| h.0 == handle) else {
        return CloverResult::UnknownHandle;
    };
    entry.1.resize(width, height);
    surface.surface.invalidate_all();
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

/// Sets the input-method composition on the focused text field.
///
/// Preview only: nothing is inserted until `clover_surface_ime_commit`.
///
/// # Safety
/// `text_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_ime_mark(
    surface: *mut CloverSurface,
    text_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if text_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(text) = CStr::from_ptr(text_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.ime_mark(text) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Commits the pending composition into the focused text field.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_ime_commit(surface: *mut CloverSurface) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if surface.surface.ime_commit() {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Discards the pending composition.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_ime_cancel(surface: *mut CloverSurface) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if surface.surface.ime_cancel() {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Inserts already-committed text at the caret (the `insertText:` path).
///
/// # Safety
/// `text_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_ime_insert(
    surface: *mut CloverSurface,
    text_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if text_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(text) = CStr::from_ptr(text_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.ime_insert(text) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Caret rectangle of the focused text field, in logical pixels.
///
/// # Safety
/// `out` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_caret_rect(
    surface: *mut CloverSurface,
    out: *mut CloverRect,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if out.is_null() {
        return CloverResult::NullArgument;
    }
    match surface.surface.focused_caret_rect() {
        Some(rect) => {
            *out = CloverRect { x: rect.x, y: rect.y, w: rect.w, h: rect.h };
            CloverResult::Ok
        }
        None => CloverResult::UnknownHandle,
    }
}

/// Reads a text field's contents into `buffer`.
///
/// Returns bytes written excluding the NUL, `-1` when the buffer is too small, or
/// `0` when the node is not a text field.
///
/// # Safety
/// `name_utf8` must be NUL-terminated and `buffer` writable for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_get_text(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    buffer: *mut c_char,
    capacity: u32,
) -> i32 {
    let Some(surface) = surface.as_mut() else { return -1 };
    if name_utf8.is_null() || buffer.is_null() || capacity == 0 {
        return -1;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return -1 };
    let Some(text) = surface.surface.text_of(name) else { return 0 };
    let bytes = text.as_bytes();
    if bytes.len() + 1 > capacity as usize {
        return -1;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, bytes.len());
    *buffer.add(bytes.len()) = 0;
    bytes.len() as i32
}

/// Replaces a text field's contents and moves the caret to the end.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_text(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    text_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() || text_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(name), Ok(text)) = (
        CStr::from_ptr(name_utf8).to_str(),
        CStr::from_ptr(text_utf8).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    if surface.surface.set_text(name, text) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Animates a node's opacity towards `target` over `duration_ms`.
///
/// # Safety
/// `name_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_animate_opacity(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    target: f32,
    duration_ms: u64,
    now_ms: u64,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.animate_opacity(name, target, duration_ms, now_ms) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Animates a node's background colour towards `rgba` (`0xRRGGBBAA`).
///
/// # Safety
/// `name_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_animate_background(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    rgba: u32,
    duration_ms: u64,
    now_ms: u64,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    let color = clover_core::Color::from_rgba_u32(rgba);
    if surface.surface.animate_background(name, color, duration_ms, now_ms) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Advances all running animations. Returns `true` while more frames are needed.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_advance(surface: *mut CloverSurface, now_ms: u64) -> bool {
    let Some(surface) = surface.as_mut() else { return false };
    surface.surface.advance(now_ms)
}

/// Registers host-owned pixels as a named image (RGBA8, straight alpha).
///
/// Hosts that already produce premultiplied BGRA8 (the frame format) should use
/// `clover_surface_add_image_bgra8_premul` instead and skip the conversion.
///
/// # Safety
/// `key_utf8` must be NUL-terminated and `pixels` must point to
/// `width * height * 4` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_add_image_rgba8(
    surface: *mut CloverSurface,
    key_utf8: *const c_char,
    width: u32,
    height: u32,
    pixels: *const u8,
) -> CloverResult {
    add_image(surface, key_utf8, width, height, pixels, false)
}

/// Registers host-owned pixels as a named image (BGRA8, premultiplied).
///
/// # Safety
/// `key_utf8` must be NUL-terminated and `pixels` must point to
/// `width * height * 4` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_add_image_bgra8_premul(
    surface: *mut CloverSurface,
    key_utf8: *const c_char,
    width: u32,
    height: u32,
    pixels: *const u8,
) -> CloverResult {
    add_image(surface, key_utf8, width, height, pixels, true)
}

unsafe fn add_image(
    surface: *mut CloverSurface,
    key_utf8: *const c_char,
    width: u32,
    height: u32,
    pixels: *const u8,
    premultiplied_bgra: bool,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if key_utf8.is_null() || pixels.is_null() || width == 0 || height == 0 {
        return CloverResult::NullArgument;
    }
    let Ok(key) = CStr::from_ptr(key_utf8).to_str() else { return CloverResult::ParseError };
    let count = width as usize * height as usize;
    let bytes = std::slice::from_raw_parts(pixels, count * 4);
    let mut words = Vec::with_capacity(count);
    if premultiplied_bgra {
        for chunk in bytes.chunks_exact(4) {
            // Memory order B,G,R,A becomes the packed premultiplied word directly.
            words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    } else {
        for chunk in bytes.chunks_exact(4) {
            let color = clover_core::Color::rgba(chunk[0], chunk[1], chunk[2], chunk[3]);
            words.push(color.premultiply().0);
        }
    }
    surface.surface.renderer_mut().images.insert_named(
        key,
        clover_render::canvas::ImagePixels { width, height, pixels: words },
    );
    CloverResult::Ok
}

/// Replaces the overlay root, which paints above the page and hit-tests first.
///
/// Menus, dropdowns, tooltips and toasts live here: an overlay is not clipped by
/// the node that opened it.
///
/// # Safety
/// `xaml_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_overlay_cui(
    surface: *mut CloverSurface,
    xaml_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if xaml_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(markup) = CStr::from_ptr(xaml_utf8).to_str() else { return CloverResult::ParseError };
    // Parsed in the page's context so an overlay may embed documents too, which is
    // how floating panels work.
    let Some(children) = surface.surface.parse_fragment(markup) else {
        return CloverResult::ParseError;
    };
    // The wrapper itself must not take hits: it covers the whole viewport and
    // would swallow every press meant for the page underneath. It still has to be
    // viewport-sized, or hit testing stops at the root and never reaches a floating
    // panel inside it.
    // Deliberately unsized: giving the wrapper an extent makes it arrange its
    // absolutely positioned children from the origin, which put floating panels at
    // (0, 0). Hit testing tests the children directly instead (see overlay_claims).
    let mut root = node::panel().hit_test(false);
    root.children = children;
    surface.surface.set_overlay(Some(root));
    CloverResult::Ok
}

/// Removes the overlay.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_clear_overlay(surface: *mut CloverSurface) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    surface.surface.set_overlay(None);
    CloverResult::Ok
}

/// Cursor the host should show at a logical point (`0` default, `1` pointer,
/// `2` text, `3` resize-h, `4` resize-v, `5` move, `6` crosshair).
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_cursor(
    surface: *mut CloverSurface,
    x: f32,
    y: f32,
) -> u32 {
    let Some(surface) = surface.as_mut() else { return 0 };
    match surface.surface.cursor_at(x, y) {
        clover_ui::style::Cursor::Default => 0,
        clover_ui::style::Cursor::Pointer => 1,
        clover_ui::style::Cursor::Text => 2,
        clover_ui::style::Cursor::ResizeHorizontal => 3,
        clover_ui::style::Cursor::ResizeVertical => 4,
        clover_ui::style::Cursor::Move => 5,
        clover_ui::style::Cursor::Crosshair => 6,
    }
}

/// Creates a surface from a `.cui` document, resources included.
///
/// Image resources are resolved relative to the document, so a shell can ship
/// next to its assets.
///
/// # Safety
/// `path_utf8` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_create_from_cui(
    width: f32,
    height: f32,
    scale: f32,
    path_utf8: *const c_char,
) -> *mut CloverSurface {
    if path_utf8.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(path) = CStr::from_ptr(path_utf8).to_str() else {
        return std::ptr::null_mut();
    };
    let Ok(document) = xaml::UiDocument::from_path(path) else {
        return std::ptr::null_mut();
    };
    let base = std::path::Path::new(path)
        .parent()
        .map(|dir| dir.to_path_buf())
        .unwrap_or_default();
    let Some(mut surface) = Surface::new(node::panel(), width, height, scale) else {
        return std::ptr::null_mut();
    };
    surface.set_base_dir(Some(base.clone()));
    for (key, relative) in document.resources() {
        let full = base.join(relative);
        if surface
            .renderer_mut()
            .images
            .load_png_named(key.clone(), &full)
            .is_err()
        {
            return std::ptr::null_mut();
        }
    }
    let root = {
        let images = &surface.renderer().images;
        match document.build(&move |source: &str| images.handle_for(source)) {
            Ok(root) => root,
            Err(_) => return std::ptr::null_mut(),
        }
    };
    let overlay = {
        let images = &surface.renderer().images;
        match document.build_overlay(&move |source: &str| images.handle_for(source)) {
            Ok(overlay) => overlay,
            Err(_) => return std::ptr::null_mut(),
        }
    };
    if let Some(overlay) = overlay {
        surface.set_overlay(Some(overlay));
    }
    surface.set_root(root);
    Box::into_raw(Box::new(CloverSurface { surface, direct: Vec::new(), pending: Vec::new() }))
}

/// Replaces a text node's content by `x:Name`.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_label(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    text_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() || text_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(name), Ok(text)) = (
        CStr::from_ptr(name_utf8).to_str(),
        CStr::from_ptr(text_utf8).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    if surface.surface.set_label(name, text) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Pushes a value into a slider or scrollbar (the MVVM update direction).
///
/// # Safety
/// `name_utf8` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_value(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    value: f32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.set_value(name, value) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Pushes a checked state into a checkbox.
///
/// # Safety
/// `name_utf8` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_checked(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    checked: bool,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.set_checked(name, checked) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Pushes a selection into a combo box.
///
/// # Safety
/// `name_utf8` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_selected(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    index: u32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.set_selected(name, index as usize) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Enables or disables a node, dimming it when disabled.
///
/// # Safety
/// `name_utf8` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_enabled(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    enabled: bool,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.set_enabled(name, enabled) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Replaces a named node's children from a markup fragment, for dynamic lists.
///
/// The fragment is wrapped in a container, so the host passes a list of elements
/// rather than a single root.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_children_xaml(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    xaml_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() || xaml_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(name), Ok(markup)) = (
        CStr::from_ptr(name_utf8).to_str(),
        CStr::from_ptr(xaml_utf8).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    let Some(children) = surface.surface.parse_fragment(markup) else {
        return CloverResult::ParseError;
    };
    if surface.surface.set_children(name, children) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Rebuilds the whole page from a `.cui` document, resources included.
///
/// # Safety
/// `path_utf8` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_reload_cui(
    surface: *mut CloverSurface,
    path_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if path_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(path) = CStr::from_ptr(path_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.reload_cui(path) {
        CloverResult::Ok
    } else {
        CloverResult::ParseError
    }
}

/// Points a named `<CloverSurface>` node at a live direct region.
///
/// # Safety
/// `name_utf8` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_bind_direct(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    handle: u64,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else { return CloverResult::ParseError };
    if surface.surface.bind_direct_surface(name, clover_ui::SurfaceHandle(handle)) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Replaces a combo box's items from a comma-separated list.
///
/// The host supplies the real list, so the popup can never disagree with the
/// model about which options exist.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_items(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    items_csv: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() || items_csv.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(name), Ok(csv)) = (
        CStr::from_ptr(name_utf8).to_str(),
        CStr::from_ptr(items_csv).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    let items: Vec<String> = csv.split(',').map(|item| item.trim().to_string()).collect();
    if surface.surface.set_items(name, items) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Opens a menu at a logical anchor, with rows reported as `<prefix>:<index>`.
///
/// Same popup as a combo box, so clamping, scrolling, hover and keyboard behave
/// identically. Hosts use it for context menus, submenus and palette flyouts.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_open_menu(
    surface: *mut CloverSurface,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    items_csv: *const c_char,
    prefix_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if items_csv.is_null() || prefix_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(csv), Ok(prefix)) = (
        CStr::from_ptr(items_csv).to_str(),
        CStr::from_ptr(prefix_utf8).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    let items: Vec<String> = csv.split(',').map(|item| item.trim().to_string()).collect();
    let anchor = clover_core::Rect::new(x, y, width.max(1.0), height.max(1.0));
    if surface.surface.open_menu(anchor, items, prefix) {
        CloverResult::Ok
    } else {
        CloverResult::ParseError
    }
}

/// Closes any open popup or menu.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_close_menu(surface: *mut CloverSurface) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    surface.surface.close_popup();
    CloverResult::Ok
}

/// Swaps the document shown by an embedded `<Content x:Name="...">` node.
///
/// The box keeps its layout; only the child document changes.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_set_content(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    path_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() || path_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(name), Ok(path)) = (
        CStr::from_ptr(name_utf8).to_str(),
        CStr::from_ptr(path_utf8).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    if surface.surface.set_content(name, path) {
        CloverResult::Ok
    } else {
        CloverResult::UnknownHandle
    }
}

/// Opens a submenu to the right of the menu that is currently open.
///
/// A row whose label ends with `>` reports its index but keeps the menu open, so
/// the host can cascade with this call.
///
/// # Safety
/// Both strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_open_submenu(
    surface: *mut CloverSurface,
    items_csv: *const c_char,
    prefix_utf8: *const c_char,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if items_csv.is_null() || prefix_utf8.is_null() {
        return CloverResult::NullArgument;
    }
    let (Ok(csv), Ok(prefix)) = (
        CStr::from_ptr(items_csv).to_str(),
        CStr::from_ptr(prefix_utf8).to_str(),
    ) else {
        return CloverResult::ParseError;
    };
    let items: Vec<String> = csv.split(',').map(|item| item.trim().to_string()).collect();
    if surface.surface.open_submenu(items, prefix) {
        CloverResult::Ok
    } else {
        CloverResult::ParseError
    }
}

/// Locks the back buffer for an *incremental* update.
///
/// The buffer starts as a copy of the published frame, so a caller that repaints
/// only a damaged region keeps the rest of the image. Publish it with
/// `clover_direct_commit_rect` so only that region is repainted.
///
/// # Safety
/// Output pointers must be valid.
#[no_mangle]
pub unsafe extern "C" fn clover_direct_begin_write_incremental(
    surface: *mut CloverSurface,
    handle: u64,
    out_pixels: *mut *mut u32,
    out_stride: *mut usize,
    out_rows: *mut u32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if out_pixels.is_null() || out_stride.is_null() || out_rows.is_null() {
        return CloverResult::NullArgument;
    }
    let Some(entry) = surface.direct.iter().find(|(h, _)| h.0 == handle) else {
        return CloverResult::UnknownHandle;
    };
    let buffer = entry.1.take_back_incremental();
    let (ptr, width, height) = (buffer.as_ptr() as *mut u32, entry.1.width(), entry.1.height());
    surface.pending.push((handle, buffer));
    *out_pixels = ptr;
    *out_stride = width as usize;
    *out_rows = height;
    CloverResult::Ok
}

/// Returns 1 when the overlay claims `(x, y)`.
///
/// A host that sends pointer events to something painted underneath — a canvas in a
/// direct region — must ask first, or clicks go through floating panels.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_overlay_hit(
    surface: *mut CloverSurface,
    x: f32,
    y: f32,
) -> i32 {
    let Some(surface) = surface.as_ref() else { return 0 };
    i32::from(surface.surface.overlay_claims(clover_core::Point::new(x, y)))
}

/// Publishes an incremental write and repaints only `(x, y, w, h)`.
///
/// Coordinates are in the page's logical pixels, so the host maps the canvas
/// damage back into its own layout.
///
/// # Safety
/// `surface` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn clover_direct_commit_rect(
    surface: *mut CloverSurface,
    handle: u64,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    let Some(index) = surface.pending.iter().position(|(h, _)| *h == handle) else {
        return CloverResult::UnknownHandle;
    };
    let (_, buffer) = surface.pending.swap_remove(index);
    let Some(entry) = surface.direct.iter().find(|(h, _)| h.0 == handle) else {
        return CloverResult::UnknownHandle;
    };
    entry.1.publish_back(buffer);
    surface
        .surface
        .invalidate_rect(clover_core::Rect::new(x, y, width.max(1.0), height.max(1.0)));
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

/// A rectangle in surface logical pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CloverRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Looks up a node by `x:Name` and reports its laid-out bounds in logical pixels.
///
/// Hosts use this to size a direct-memory region to the box the layout actually
/// gave it, instead of guessing.
///
/// # Safety
/// `name_utf8` must be a valid NUL-terminated string and `out` a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn clover_surface_node_bounds(
    surface: *mut CloverSurface,
    name_utf8: *const c_char,
    out: *mut CloverRect,
) -> CloverResult {
    let Some(surface) = surface.as_mut() else { return CloverResult::NullArgument };
    if name_utf8.is_null() || out.is_null() {
        return CloverResult::NullArgument;
    }
    let Ok(name) = CStr::from_ptr(name_utf8).to_str() else {
        return CloverResult::ParseError;
    };
    match surface.surface.tree().named(name) {
        Some(node) => {
            *out = CloverRect { x: node.bounds.x, y: node.bounds.y, w: node.bounds.w, h: node.bounds.h };
            CloverResult::Ok
        }
        None => CloverResult::UnknownHandle,
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
