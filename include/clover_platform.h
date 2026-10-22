/*
 * CloverPlatform C ABI
 * ====================
 *
 * Embed the CloverPlatform UI + CPU renderer in a native host (C, C++,
 * Objective-C++). The host owns the window and the event loop; CloverPlatform
 * owns the UI tree, layout, rasterisation and the frame buffer.
 *
 * Threading contract
 * ------------------
 *   * A `clover_surface*` is single-threaded: create, resize, push events and
 *     read frames from one thread (normally the UI thread).
 *   * `clover_direct_*` regions are the exception. They are designed so a
 *     dedicated thread (CloverPic's paint canvas) can write pixels straight into
 *     memory at its own refresh rate. The UI thread keeps presenting the last
 *     committed frame until `clover_direct_commit` publishes the new one, which
 *     is the swap-chain consistency guarantee.
 *
 * Pixel format
 * ------------
 * Frames are premultiplied BGRA8 packed in `uint32_t` (little-endian memory
 * order B, G, R, A). This matches CloverPic's existing `RgbaFrame`.
 */

#ifndef CLOVER_PLATFORM_H
#define CLOVER_PLATFORM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Result codes. */
typedef enum clover_result {
    CLOVER_OK = 0,
    CLOVER_NULL_ARGUMENT = 1,
    CLOVER_PARSE_ERROR = 2,
    CLOVER_NO_FONT = 3,
    CLOVER_UNKNOWN_HANDLE = 4
} clover_result;

/* Key actions. */
typedef enum clover_key_action {
    CLOVER_KEY_DOWN = 0,
    CLOVER_KEY_UP = 1
} clover_key_action;

/* Pointer actions. */
typedef enum clover_pointer_action {
    CLOVER_POINTER_MOVE = 0,
    CLOVER_POINTER_DOWN = 1,
    CLOVER_POINTER_UP = 2,
    CLOVER_POINTER_CANCEL = 3
} clover_pointer_action;

/* Modifier bits, matching CloverPic's Input::ModifierKeys. */
enum {
    CLOVER_MODIFIER_SHIFT = 1 << 0,
    CLOVER_MODIFIER_CONTROL = 1 << 1,
    CLOVER_MODIFIER_ALT = 1 << 2
};

typedef struct clover_surface clover_surface;

/* A presented frame. `pixels` stays valid until the next render of this surface. */
typedef struct clover_frame {
    const uint32_t *pixels;
    uint32_t width;
    uint32_t height;
    size_t stride;       /* in pixels, not bytes */
    bool premultiplied;  /* always true today */
    uint64_t generation; /* increments on every rendered frame */
} clover_frame;

/* A rectangle in surface logical pixels. */
typedef struct clover_rect {
    float x;
    float y;
    float w;
    float h;
} clover_rect;

typedef struct clover_stats {
    uint32_t nodes;
    uint32_t tiles_last_frame;
    uint32_t width;
    uint32_t height;
    float scale;
} clover_stats;

/*
 * Lifecycle
 * ---------
 * `width`/`height` are logical pixels; `scale` is the display scale factor
 * (2.0 on a Retina display). Physical size is width*scale by height*scale.
 */

/* Parses `xaml_utf8` and creates a surface. Returns NULL on failure. */
clover_surface *clover_surface_create_from_xaml(float width, float height, float scale,
                                                const char *xaml_utf8);

/*
 * Creates a surface from a XAML page plus a `<Styles>` document. The stylesheet
 * provides defaults; attributes written on the page override it.
 */
clover_surface *clover_surface_create_from_xaml_with_styles(float width, float height, float scale,
                                                            const char *xaml_utf8,
                                                            const char *styles_utf8);

/*
 * Replaces the UI tree with a freshly parsed page. `styles_utf8` may be NULL.
 * Focus, press state and the action queue are reset because node ids change.
 */
clover_result clover_surface_set_root_xaml(clover_surface *surface, const char *xaml_utf8,
                                           const char *styles_utf8);

/*
 * `.cui` documents
 * ----------------
 * A `.cui` file embeds data, styles and one page root, so a whole UI lives in one
 * file:
 *
 *   <Ui>
 *     <Data><Value Key="title" Text="Hello"/></Data>
 *     <Styles><Style TargetType="Button" Background="#2E6FD0"/></Styles>
 *     <StackPanel><TextBlock Text="{Binding title}"/></StackPanel>
 *   </Ui>
 *
 * Bindings resolve at build time; call set_root_cui again to refresh them.
 */
clover_surface *clover_surface_create_from_cui(float width, float height, float scale,
                                               const char *path_utf8);

clover_result clover_surface_set_root_cui(clover_surface *surface, const char *path_utf8);

/* Creates a surface with an empty root panel. */
clover_surface *clover_surface_create_empty(float width, float height, float scale);

void clover_surface_destroy(clover_surface *surface);

clover_result clover_surface_resize(clover_surface *surface, float width, float height, float scale);

/* Renders dirty regions and copies the frame descriptor into `out`. */
clover_result clover_surface_frame(clover_surface *surface, clover_frame *out);

/* Marks the whole surface dirty. */
clover_result clover_surface_invalidate(clover_surface *surface);

clover_result clover_surface_stats(clover_surface *surface, clover_stats *out);

/*
 * Input
 * -----
 * `button`: 0 none, 1 left, 2 right, 3 middle.
 * Returns the hit node id, or 0 when nothing was hit.
 */
uint64_t clover_surface_pointer(clover_surface *surface, float x, float y,
                                clover_pointer_action action, uint32_t button,
                                uint32_t modifiers);

/*
 * Keyboard and focus
 * ------------------
 * Tab / Shift+Tab move focus before the event is routed. Key events then route
 * capture -> target -> bubble along the focused node's ancestors.
 */

/* Returns the focused node id after routing, or 0. */
uint64_t clover_surface_key(clover_surface *surface, uint32_t key, clover_key_action action,
                            uint32_t modifiers, bool repeat);

/* Wheel / trackpad scroll. Positive delta is wheel-up. */
clover_result clover_surface_wheel(clover_surface *surface, float x, float y, float delta,
                                   uint32_t modifiers);

/*
 * Actions
 * -------
 * A node with `Command="id"` (or `Node::action`) reports `id` when clicked, or
 * when Enter/Space is pressed while a descendant has focus. Drain after input.
 */

/* Returns bytes written excluding the NUL, 0 when empty, -1 on error. */
int32_t clover_surface_take_action(clover_surface *surface, char *buffer, uint32_t capacity);

/*
 * MVVM update direction
 * ---------------------
 * The host pushes model changes into the tree instead of rebuilding it. Each call
 * damages only the node it touches and returns CLOVER_OK when the value actually
 * changed.
 */
clover_result clover_surface_set_value(clover_surface *surface, const char *name_utf8, float value);
clover_result clover_surface_set_checked(clover_surface *surface, const char *name_utf8, bool checked);
clover_result clover_surface_set_selected(clover_surface *surface, const char *name_utf8,
                                          uint32_t index);
clover_result clover_surface_set_enabled(clover_surface *surface, const char *name_utf8, bool enabled);

/* Replaces a named node's children from a fragment, for dynamic lists. */
clover_result clover_surface_set_children_xaml(clover_surface *surface, const char *name_utf8,
                                               const char *xaml_utf8);

/* Rebuilds the whole page from a .cui document, resources included. */
clover_result clover_surface_reload_cui(clover_surface *surface, const char *path_utf8);

/* Replaces a text node's content by x:Name. */
clover_result clover_surface_set_label(clover_surface *surface, const char *name_utf8,
                                       const char *text_utf8);

/* Shows or hides a node by x:Name and repaints it. */
clover_result clover_surface_set_visible(clover_surface *surface, const char *name_utf8,
                                         bool visible);

/* Returns the focused node id, or 0. */
uint64_t clover_surface_focus(clover_surface *surface);

/* Moves focus forward (or backward) and returns the new focused node id. */
uint64_t clover_surface_focus_next(clover_surface *surface, bool reverse);

/*
 * Direct-memory regions
 * ---------------------
 * Registers a child surface and returns its handle (never 0). Reference it from
 * XAML with `<CloverSurface Id="HANDLE"/>` or from Rust with
 * `clover_ui::node::direct_surface`.
 */
uint64_t clover_surface_add_direct(clover_surface *surface, uint32_t width, uint32_t height);

/*
 * Locks the back buffer and returns it for writing. The pointer remains valid
 * until `clover_direct_commit`, which publishes the frame.
 *
 * `out_stride` is in pixels; `out_height` is the buffer height.
 */
clover_result clover_direct_begin_write(clover_surface *surface, uint64_t handle,
                                        uint32_t **out_pixels, size_t *out_stride,
                                        uint32_t *out_height);

clover_result clover_direct_commit(clover_surface *surface, uint64_t handle);

/* Binds a named <CloverSurface> node to a live direct region. */
clover_result clover_surface_bind_direct(clover_surface *surface, const char *name_utf8,
                                         uint64_t handle);

/* Resizes a direct region; contents are cleared. Call when layout changes. */
clover_result clover_direct_resize(clover_surface *surface, uint64_t handle, uint32_t width,
                                   uint32_t height);

/* Looks up a node by x:Name and reports its laid-out bounds in logical pixels. */
clover_result clover_surface_node_bounds(clover_surface *surface, const char *name_utf8,
                                         clover_rect *out);

/*
 * Text fields
 * -----------
 * `<TextBox x:Name="search" Text="{Binding query}" Placeholder="Search"/>` is
 * editable: clicking it focuses it, and typing edits the buffer. The host is told
 * through `text:<name>` / `submit:<name>` actions and reads the value back here.
 */
int32_t clover_surface_get_text(clover_surface *surface, const char *name_utf8, char *buffer,
                                uint32_t capacity);

clover_result clover_surface_set_text(clover_surface *surface, const char *name_utf8,
                                      const char *text_utf8);

/*
 * Input methods (IME)
 * -------------------
 * A text field can show *preview* text from an input method before it is
 * committed. The platform adapter implements its native input client and maps:
 *
 *   setMarkedText   -> clover_surface_ime_mark      (preview, underlined)
 *   insertText      -> clover_surface_ime_insert    (already resolved)
 *   unmarkText      -> clover_surface_ime_cancel
 *   candidate popup -> clover_surface_caret_rect    (logical pixels)
 */
clover_result clover_surface_ime_mark(clover_surface *surface, const char *text_utf8);
clover_result clover_surface_ime_commit(clover_surface *surface);
clover_result clover_surface_ime_cancel(clover_surface *surface);
clover_result clover_surface_ime_insert(clover_surface *surface, const char *text_utf8);
clover_result clover_surface_caret_rect(clover_surface *surface, clover_rect *out);

/*
 * Animation
 * ---------
 * Opacity and background colour can be eased towards a target. The host calls
 * `clover_surface_advance` every frame; it returns true while anything is still
 * moving, so the host can stop requesting frames afterwards.
 */
clover_result clover_surface_animate_opacity(clover_surface *surface, const char *name_utf8,
                                             float target, uint64_t duration_ms, uint64_t now_ms);
clover_result clover_surface_animate_background(clover_surface *surface, const char *name_utf8,
                                                uint32_t rgba, uint64_t duration_ms, uint64_t now_ms);
bool clover_surface_advance(clover_surface *surface, uint64_t now_ms);

/*
 * Overlay
 * -------
 * The overlay root paints above the page and takes hits first, so menus,
 * dropdowns, tooltips and toasts are never clipped by whatever opened them.
 */
clover_result clover_surface_set_overlay_cui(clover_surface *surface, const char *xaml_utf8);
clover_result clover_surface_clear_overlay(clover_surface *surface);

/* Cursor the host should show at a logical point (0 default, 1 pointer, ...). */
uint32_t clover_surface_cursor(clover_surface *surface, float x, float y);

/*
 * Host-owned pixels
 * -----------------
 * Thumbnails change every frame, so they are uploaded rather than decoded from
 * disk. Registering the same key again replaces the previous image.
 */
clover_result clover_surface_add_image_rgba8(clover_surface *surface, const char *key_utf8,
                                             uint32_t width, uint32_t height, const uint8_t *pixels);
clover_result clover_surface_add_image_bgra8_premul(clover_surface *surface, const char *key_utf8,
                                                    uint32_t width, uint32_t height,
                                                    const uint8_t *pixels);

/* Registers a PNG image and returns its handle, or 0 on failure. */
uint64_t clover_surface_add_image_png(clover_surface *surface, const char *path_utf8);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CLOVER_PLATFORM_H */
