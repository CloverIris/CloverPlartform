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

## 5. Text

Shaping is per line: `wrap_lines` performs greedy word wrapping at a maximum
width and hard-breaks words wider than the line, then each line is shaped
independently against a shared glyph cache.

Measure and paint **must** call the same `wrap_lines` with the same width, or
layout and rasterisation disagree and text overflows its box. That is why
`LayoutMetrics::measure_text` takes the optional max width instead of leaving
wrapping to the renderer.

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

Known limits: no selection, no clipboard, no IME, no horizontal scrolling of
long contents, and the caret does not blink.

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
