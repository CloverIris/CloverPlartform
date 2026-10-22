# CloverPlatform

A cross-platform UI and rendering framework written in Rust, with a CPU-first
renderer and a C ABI for native hosts. It is the UI substrate for
[CloverPic](https://github.com/CloverIris/CloverPic) and is developed as its own
repository so it can be reused elsewhere.

![CloverPlatform demo: XAML toolbar, declarative shell, direct-memory canvas](docs/screenshot.png)

![The same layout scrolled four wheel notches](docs/screenshot-scroll.png)

> Status: **0.3 — scrolling**. The pipeline works end to end (tree →
> layout → display list → tile-parallel raster → frame), both authoring
> front-ends produce the same tree, layout supports flex-style `Fill`, text
> wraps, and keyboard focus is routed. Styles, templates, bindings and animation
> are not implemented yet; see [Roadmap](#roadmap).

## What it does

* **One tree, two authoring modes.** XAML (WPF/Avalonia/WinUI flavour) and
  Compose/Flutter-style declarative builders both produce the same `Node` tree,
  so they can be mixed anywhere, and a retained-mode shell can host declarative
  regions.
* **Retained and declarative in the same layout.** The mode is per-node.
* **CPU renderer with a parallel tile rasteriser.** Damage-driven: only the
  rectangles that changed are redrawn, split across cores with rayon.
* **Flex layout.** `Auto` / `Px` / `Percent` / `Fill`, main-axis distribution
  with `justify`, cross-axis alignment with `align`. Explicit sizes always win
  over stretch, matching WPF.
* **Text and images.** TrueType shaping with a glyph cache, word wrapping and
  hard breaks for over-long words; PNG decoding; antialiased rectangles, rounded
  rectangles and borders.
* **Keyboard focus.** Tab / Shift+Tab traversal over focusable nodes, and key
  events routed through capture → target → bubble along the focused node.
* **Scrolling.** Vertical scroll containers (`ScrollViewer`, `Node::scrollable`)
  clamp their offset against the laid-out content and paint a scrollbar. Wheel
  events route to the nearest scrollable ancestor.
* **Interaction feedback.** Hovered surfaces lighten and pressed ones darken by
  default, with no per-widget code.
* **Hosts.** A C ABI, exercised by CloverPic's macOS adapter; see
  [docs/MIGRATION.md](docs/MIGRATION.md).
* **Direct-memory child surfaces.** A region of the UI can be handed to another
  thread as a raw pixel buffer with swap-chain semantics — this is how a paint
  canvas keeps its own high-refresh-rate loop.
* **C ABI.** Embed the whole thing in a C/C++/Objective-C++ host.

## Layout

```text
crates/
  clover-core/      geometry, colour (straight + premultiplied), damage, threading
  clover-ui/        node tree, two authoring modes, layout, XAML front-end, events
  clover-render/    CPU canvas, text, images, display list, tile renderer, surfaces
  clover-abi/       C ABI (cdylib + staticlib)
  clover-demo/      headless demo that renders a mixed UI to PNG
include/
  clover_platform.h C header for native hosts
tests/
  abi_smoke.cpp     C++ host smoke test
```

## Build and test

```bash
cargo test                     # unit tests for every crate
cargo run -p clover-demo       # renders demo.png and prints timings
```

C ABI smoke test (from the repository root):

```bash
cargo build -p clover-abi
clang++ -std=c++20 tests/abi_smoke.cpp -I include \
        target/debug/libclover_abi.a -o abi_smoke \
        -framework Security -framework CoreFoundation
./abi_smoke
```

## Hello world

Declarative:

```rust
use clover_core::{Color, Edges};
use clover_ui::node;

let page = node::column()
    .padding(Edges::all(16.0))
    .gap(8.0)
    .background(Color::rgb(0x1E, 0x20, 0x23))
    .child(node::text("Hello").font_size(20.0))
    .child(node::button("OK"));
```

XAML:

```rust
use clover_ui::xaml;

let page = xaml::parse(r##"
<StackPanel Padding="16" Spacing="8" Background="#1E2023">
    <TextBlock Text="Hello" FontSize="20"/>
    <Button Content="OK"/>
</StackPanel>"##, &xaml::NoImages)?;
```

A canvas region that another thread owns:

```rust
node::direct_surface(handle).size(Length::Fill, Length::Fill)
```

The two authoring modes are interchangeable: a XAML page can contain
declaratively built children (`Node::child`) and vice versa. A retained-mode shell can mount a
declarative subtree at runtime:

```rust
let mount = tree.mount(shell_id, "toolbar", Box::new(|| build_toolbar()));
tree.invalidate_mount(mount);
tree.flush();   // reconciles by key, reusing node ids
```

## Rendering model

```text
UiTree ──layout──► absolute bounds ──build──► DisplayList (physical px)
                                                  │
                                    damage rects ─┴─► tiles ──rayon──► Canvas
                                                                        │
                                                          DirectSurface ─┘ composite
```

* Layout is a small flexbox: measure content, resolve `Auto`/`Px`/`Percent`,
  hand leftover main-axis space to `Fill` children, distribute the rest with
  `justify`, then place on the cross axis with `align`. Explicit sizes always
  win over `Stretch`.
* The display list is resolved in **physical pixels** and carries shaped glyphs
  and decoded images, so tile rendering touches no shared mutable state.
* Frames are premultiplied BGRA8 in `u32`, the same layout CloverPic's
  `RgbaFrame` uses.

## Roadmap

Near term:

- text runs with mixed styles (bold spans, ellipsis, alignment per run),
- XAML styles, templates, property elements and `{Binding}`,
- focus visuals: the framework routes focus, skins still draw it,
- horizontal scrolling and scroll-into-view on focus,
- region-level opacity layers.

Later:

- scroll containers and virtualised lists,
- animation scheduler with frame pacing,
- GPU (Metal / D3D) backend behind the same `DrawCmd` stream,
- separate surface threads per subtree (the direct-surface model generalised).

## License

MIT. See [LICENSE](LICENSE).
