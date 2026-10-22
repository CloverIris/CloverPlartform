# Architecture

This document records the decisions that are expensive to change later.

## 1. One tree, two front-ends

`clover_ui::Node` is the single UI representation. XAML parsing and the
declarative builders are **front-ends** that construct it; nothing downstream
knows which one produced a node.

The alternative — separate retained and retained/declarative models — was
rejected because the requirement is explicitly *mixing*: a XAML toolbar inside a
retained editor shell, with a Compose-style panel inside the toolbar. Any design
with two models needs a bridge at every boundary.

`NodeMode` is therefore per node:

| mode | owner | lifetime |
|---|---|---|
| `Retained` | host, mutated by `NodeId` | until the host changes it |
| `Declarative` | a builder registered on `UiTree` | rebuilt on `invalidate_mount` + `flush` |

A declarative mount point is an ordinary node that hosts the builder's children,
so retained siblings and declarative subtrees interleave freely. Reconciliation
matches children by `key` and preserves `NodeId` and interaction state.

## 2. Layout produces absolute bounds

Every node stores its bounds in surface space, not parent-relative.

* Hit testing becomes a single `Rect::contains` per level.
* Damage and the display list need no ancestor transform accumulation.
* The cost is one extra add per child during arrange — irrelevant next to
  everything else.

Transforms exist (`clover_core::Transform`) for scrolled/zoomed subtrees, but the
current layout path does not emit them yet.

### Flex distribution

`Length::Fill` cannot be resolved during measure: it depends on what the siblings
need. Arrange therefore runs in two steps — measure everything, subtract the
fixed children (plus gaps) from the main axis, then split the remainder between
the `Fill` children.

Two consequences worth keeping in mind:

* Over-subscribed rows give `Fill` children zero width rather than negative.
* `Fill` on the cross axis is treated as stretch, so `Width::Fill` inside a
  column makes a full-width row and `Height::Fill` inside a row makes a
  full-height column. There is no separate "grow" and "stretch" vocabulary.

Justify still runs afterwards, on whatever slack is left, so a layout can mix
`Fill` with `justify: Center` without special cases.

## 2b. Layout is gated, and that is load-bearing

`UiTree::layout` returns immediately unless `layout_dirty` is set (resize, new
root, mount flush, visibility change). This looks like a micro-optimisation and
is not:

The pass marks the whole root dirty. Early versions ran layout on every frame,
so damage tracking was permanently defeated and every frame became a
full-surface repaint. Measured at 3000x1700: a hover repaint cost **12.03 ms**
before the gate and **0.160 ms** after — a 73x difference for a one-line rule.

The lesson generalises: damage tracking is not a feature, it is a discipline. Any
step that unconditionally marks geometry dirty silently reverts the whole
pipeline to full-frame rendering.

`cargo run --release -p clover-render --example bench_surface` measures both
paths (7.0 ms full frame vs 0.160 ms hover at 3000x1700) and is the regression
guard.

## 3. The display list is resolved, not deferred

`DisplayList` contains *ready-to-blit* work: shaped glyph bitmaps (`Arc`),
premultiplied decoded images (`Arc`), premultiplied colours, and physical-pixel
rectangles.

This is what makes parallel rasterisation safe and lock-free: the tile pass is a
pure function from `(DisplayList, Rect) -> Canvas`. No font cache, no image
registry, no tree walk on the worker threads.

The alternative — resolving glyphs during rasterisation — would need a mutex
around the glyph cache, which serialises every text draw.

## 4. Damage-driven tiles

`Damage` coalesces dirty rects (tiny updates merge into their neighbour). Each
rect is split into tiles of at most 256×256 physical pixels and rendered with
`rayon::par_iter`.

Each tile renders into its own small canvas, which is then copied into the frame.
That costs one extra copy of the damaged area and buys a completely safe parallel
section. If profiling ever says the copy dominates, the fix is a
`split_at_mut`-based stripe renderer behind the same interface.

## 4b. Text

Three libraries, each doing one job:

| job | library |
|---|---|
| shaping | rustybuzz (a HarfBuzz port) |
| line breaks | unicode-linebreak (UAX #14) |
| outlines | ttf-parser + ab_glyph_rasterizer |

Shaping per character — the previous approach — cannot produce ligatures, kerning
or mark positioning, and it cannot render scripts that reorder glyphs. Replacing
it was not optional.

Font fallback is **per character**: text is segmented into runs by the first face
that has a glyph for each character, and each run is shaped with its own face.
That is what makes mixed Latin/CJK labels work, and it is why `.ttc` collections
matter — `HelveticaNeue.ttc` and `Hiragino Sans GB.ttc` are the faces that ship
with macOS.

Faces are parsed on demand from `Arc<Vec<u8>>` rather than stored as
self-referential structs. Parsing a face is a header scan, and both shaping and
rasterisation already happen per run or per glyph-cache-miss, so the cost is
noise next to shaping itself.

Caret positions and wrapping both call the same shaping path, so the caret can
never drift from the glyphs. Wrapping measures a shaped paragraph once and maps
byte offsets to pen positions through glyph clusters, rather than re-shaping every
candidate prefix.

Known limits: no IME (typing Chinese into a text field still requires a platform
bridge), no bidi run reordering for mixed-direction paragraphs, no variable font
axes, no colour emoji.

## 5. Text

Shaping is per line: `wrap_lines` performs greedy word wrapping at a maximum
width and hard-breaks words wider than the line, then each line is shaped
independently against a shared glyph cache.

Measure and paint **must** call the same `wrap_lines` with the same width, or
layout and rasterisation disagree and text overflows its box. That is why
`LayoutMetrics::measure_text` takes the optional max width instead of leaving
wrapping to the renderer.

## 5b. Render context

`RenderContext` is the seam a GPU backend will plug into: format, colour space,
scale, viewport, and later the command-recording state. The CPU renderer is one
consumer of it.

Format negotiation is a separate, explicit step rather than something the
renderer decides quietly:

```text
FormatRequest (host wants HDR / wide gamut)
  + DisplayCapabilities (what the display accepts)
  + hdr_raster (what this build can actually paint)
    -> FormatChoice { format, color_space, fallback: Option<Reason> }
```

Today `hdr_raster` is false, so an HDR request lands on `Bgra8Sdr` with
`FallbackReason::HdrRasterUnavailable`. That is a *reported* fallback, not a
silent one — the whole point of recording the reason is that a host can surface
it and a future float raster path only has to flip one flag.

## 5b1. Vector paths

Icons cannot be bitmaps: the toolbar draws at 1x and 2x, and an exported PNG is
wrong at every scale but one.

`clover-core::path` parses SVG path data (`M L H V C S Q T Z`, absolute and
relative) into a flat command list with cached bounds. `clover-render::path`
rasterises it with **the same rasteriser as text** — `ab_glyph_rasterizer`, which
produces analytic coverage. That is why icon edges look identical to glyph edges
rather than merely similar.

Fill closes open subpaths, matching SVG. Stroke flattens curves and draws a quad
per segment plus a twelve-sided round join: solid corners at any width without
implementing mitre joins. Arcs (`A`) are not supported yet; the icons here are
built from curves.

## 5c1. Overlay

Popups cannot live in the page tree: a dropdown opened from a toolbar button would
be clipped by the toolbar, and stacking would be decided by declaration order
rather than by "this is on top".

So a document may declare `<Overlay>`, built as a second root. It paints after the
page and is hit-tested before it, so a hit inside the overlay never reaches the
page beneath.

The overlay root itself spans the viewport but is marked `hit_test_visible =
false`: it paints nothing and takes no hits, so a click beside the menu still
reaches the page. Only the elements declared inside it take input. `IsHitTestVisible`
exposes the same flag for ordinary nodes.

## 5c2. Absolute positioning and drag

Two capabilities were added because the CloverPic workspace cannot be expressed
without them:

**Absolute positioning.** `Style::position` takes a node out of the flow. A
margin shifts a node inside its slot; it cannot make the node ignore the slot.
Floating panels, menus and tooltips need the latter, so positioned children are
measured and placed first, then excluded from every flow calculation (content
size, fill distribution, gap accounting, scroll extent).

**Drag.** The framework owns the gesture, the host owns the meaning:
`drag:begin:<payload>`, `drag:move:<payload>:<dx>:<dy>`, `drag:end:<payload>`.
Deltas are incremental and in logical pixels, so a host can apply them to its own
state without tracking the origin.

A press on a drag handle claims the gesture before press/action logic runs, so
dragging a panel never also activates whatever is drawn on it.

## 5d. Gradients

A gradient is a brush, not a separate draw path: `Brush::Gradient { geometry,
stops }` renders through the same command as a textured or solid background, and
the rasteriser antialiases it with the rounded-rect SDF. Linear geometry is an
angle across the box, radial is a normalised centre and reach, so a `.cui` file
never carries pixel coordinates that break on resize.

## 5e. Templates

A template is stored as raw markup inside the stylesheet and instantiated once
per control. That keeps `StyleSheet` a plain value type, lets a template be
re-parsed with the control's own data context (`Content`, `Tag`, `Name`), and
means templates compose with bindings for free.

When a control carries `Template=`, it stops drawing its own background, border
and label: the template owns the visual. The layout box still belongs to the
control, so a template cannot change where the control sits.

## 5f. Animation

Animations live on the node (`Node::animation`) rather than in a global
timeline, so the display list needs no clock: `Surface::advance(now_ms)` writes
the eased value into `Style` and damages the node.

`advance` returns whether anything is still running, which is what lets a host
stop asking for frames. Smoothstep easing is applied once, in `Animation::progress`.

## 5c. StyleBrush

A background is either a solid colour or a texture. Solid stays a plain colour so
the common path never touches the image pipeline; textures carry an
`ImageHandle` resolved while the page is built, so the renderer never sees a path
or a resource name.

`.cui` declares images in `<Resources>` with paths relative to the document, and
the ABI resolves and registers them before building. A background texture is
clipped to its own control, so `Cover` crops instead of overflowing into
neighbours.

## 6. UI documents

A `.cui` file is `<Ui>` wrapping three optional sections: `<Data>`, `<Styles>`,
and exactly one page root. Parsing produces a `UiDocument` that keeps the data
and the sheet; `build` produces a `Node` tree with bindings resolved.

Keeping the parsed document around matters: refreshing a page after data changes
is `document.build_with(&overrides)` rather than re-reading and re-parsing the
file, and hosts can layer runtime values (`overrides`) over the document's
defaults without editing it.

Binding is deliberately not reactive. There is no observer graph and no
invalidation: the host rebuilds the page when its data changes. For a desktop app
that repaints on input anyway, a pull model is both simpler and cheaper than
pushing change notifications through a tree.

## 7. Styles

A stylesheet is an ordered list of `(selector, partial properties)` rules. Only
three selector forms exist — universal, element type, and `x:Name` — which is
enough to theme a control set without writing a CSS engine.

Application order is specificity first, document order second, so a name selector
always beats a type selector no matter where it appears. Styles are applied
*before* element attributes, and attributes always win: the stylesheet is a set
of defaults, never an override, which keeps the markup readable when one element
needs to differ.

Font size and text colour cascade explicitly through a build-time context rather
than by re-walking the tree. Inheritance flows down; anything else stays local.

## 8. Scrolling

A scroll container is a **viewport**; that word is doing real work. During
measure it reports `min(content, available)` instead of its content height.

Without that rule, `Height: Fill` inside an `Auto`-height parent resolves against
the content height the container just reported, so the container ends up exactly
as tall as its content and can never scroll. The bug is invisible in a simple
test and obvious the moment a scroll view sits in a stack.

During arrange the container measures its content, stores it on
`Node::scroll.content_height`, clamps `offset` to `content - viewport`, and lays
children out in a box shifted up by the offset. Wheel events route to the nearest
scrollable ancestor of the hit node; the scrollbar is emitted after the children
so it paints on top.

## 9. Text fields

A text field owns its buffer and caret in the tree, not in the host. Editing keys
are consumed by `Surface::dispatch` before the normal key path, so the widget
needs no event plumbing of its own.

The host learns about edits through actions (`text:<name>`, `submit:<name>`) and
reads the value back with `clover_surface_get_text`. That keeps the ownership
clear: the framework holds transient editing state, the host holds the meaning.

### Input methods

Composing text is a three-state cycle, and the state lives in the tree:

| platform call | framework call | effect |
|---|---|---|
| `setMarkedText:` | `ime_mark` | preview inserted at the caret, underlined |
| `insertText:` | `ime_insert` | already-resolved text inserted, composition cleared |
| `unmarkText` | `ime_cancel` | preview dropped |

The composition is *never* part of the field's `text` until committed, which is
what lets an IME replace or abandon a phrase without the host seeing partial
keystrokes. The caret, the underline and the candidate-window anchor all read the
same composed string, so they cannot disagree.

On macOS the adapter implements `NSTextInputClient` on a shared `CloverImeView`
base: `interpretKeyEvents:` routes keys through the system input source, and
`doCommandBySelector:` maps editing commands back onto the framework's
virtual-key path so there is still only one editing implementation.

Known limits: no selection, no clipboard, no bidi run reordering, no horizontal
scrolling of long contents, the caret does not blink, and `markedRange` /
`selectedRange` report nothing — an IME that asks where the marked text sits gets
no answer, so candidate placement relies on the caret rectangle alone.

## 10. Actions

A node can carry a string command (`x:Name`-style authoring: `Command="save"`).
Activation is the framework's job; meaning is the host's.

Activation rules, all of which fall out of `press_action`:

- pointer-down records the nearest actionable ancestor of the hit node,
- pointer-up only fires when it resolves to the same node, so dragging off a
  button cancels it,
- Enter or Space fires the focused node's nearest actionable ancestor.

Actions are queued, never dispatched from inside the input call. The host drains
them after pushing input, which keeps re-entrancy out of the framework: an action
handler can mutate the tree (`clover_surface_set_visible`) without the input
stack pointing at stale nodes.

## 11. Focus

Focus lives on `UiTree`, not on the platform layer: `set_focus` and `focus_next`
update `NodeState::focused` and mark the affected bounds dirty. Traversal order is
depth-first over visible, focusable nodes.

Pointer-down focuses the innermost focusable ancestor of the hit node, and
Tab/Shift+Tab are consumed by `Surface::dispatch` before routing. Everything else
routes from the focused node.

## 12. Direct-memory surfaces

A `DirectSurface` is a pixel buffer that another thread owns and writes.

```
writer thread                       UI thread
-------------                       ---------
take_back()  ── owned Vec<u32> ──►  front buffer keeps presenting
   write pixels
publish_back() ─────────────────►    swap: zero copies
```

`take_back` removes the buffer from the surface; `publish_back` swaps it into the
front slot and returns the old front as the next back buffer. No allocation and
no pixel copy per frame — only the mutex is taken, for the duration of the swap
rather than the write.

The UI thread composites the surface into its tile renderer by sampling the front
buffer, so a canvas that has not committed a new frame keeps showing the previous
one. That is the swap-chain consistency guarantee.

The C ABI exposes this as `clover_direct_begin_write` / `clover_direct_commit`,
which is exactly the "give me the memory address" escape hatch.

## 13. Pixel format

Internally, and on the frame handed to hosts: **premultiplied BGRA8 in `u32`**
(`0xAARRGGBB`, memory order B,G,R,A on little-endian).

Premultiplied because source-over then costs one multiply-add per channel and is
associative, so tiles can be composed in any order. Un-premultiplication happens
only at export boundaries (PNG).

This also matches CloverPic's `RgbaFrame`, so a CloverPlatform surface can be
handed to the existing adapters without a conversion pass.

## 14. C ABI boundary

The host keeps: window creation, the event loop, platform input, and presenting
pixels. CloverPlatform keeps: the tree, layout, rasterisation and the frame
buffer.

That split is deliberate: it means the framework never needs a platform layer of
its own, and CloverPic's already-working Windows and macOS adapters can adopt it
one surface at a time.

Handles are opaque; every entry point validates null and returns a
`clover_result`. `CloverFrame::pixels` is valid until the next render of that
surface.

## Open questions

* **Horizontal scrolling and scroll-into-view.** Only the vertical axis scrolls,
  and focus does not reveal an off-screen node yet.
* **Mixed-style text runs.** Wrapping currently assumes one font size and one
  colour per node, so a bold span forces a whole extra node.
* **Templates and bindings.** Styles cover values but not structure: there is no
  control template, and no data binding yet.
* **Animation.** No scheduler yet; the host decides when to ask for a frame.
