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
v0.1 layout path does not emit them yet.

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

## 5. Direct-memory surfaces

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

## 6. Pixel format

Internally, and on the frame handed to hosts: **premultiplied BGRA8 in `u32`**
(`0xAARRGGBB`, memory order B,G,R,A on little-endian).

Premultiplied because source-over then costs one multiply-add per channel and is
associative, so tiles can be composed in any order. Un-premultiplication happens
only at export boundaries (PNG).

This also matches CloverPic's `RgbaFrame`, so a CloverPlatform surface can be
handed to the existing adapters without a conversion pass.

## 7. C ABI boundary

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

* **Focus and keyboard.** `clover_ui::event::route` handles key events for a
  focused `NodeId`, but nothing sets focus yet. Needs a focus scope/tab-order
  model before CloverPic's text fields can migrate.
* **Text layout.** Shaping is per-line with no line breaking. Paragraph layout is
  the next significant piece.
* **Styles.** XAML attributes are read verbatim per element. A style/template
  layer needs selector matching before `{Binding}` makes sense.
