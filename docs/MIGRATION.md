# Hosting CloverPlatform in a native app

This is the pattern CloverPic's macOS adapter uses today
(`src/Platform/Mac/MacCloverDemo.mm`). It is additive: the host can keep its
existing rendering path and opt in one surface at a time.

## 1. Build the C ABI

CloverPlatform ships as a Rust static library with a C header:

```cmake
add_custom_target(CloverPlatformAbi ALL
    COMMAND cargo build --release
            --manifest-path "${CMAKE_SOURCE_DIR}/CloverPlatform/Cargo.toml" -p clover-abi
    BYPRODUCTS "${CMAKE_SOURCE_DIR}/CloverPlatform/target/release/libclover_abi.a")

add_dependencies(YourApp CloverPlatformAbi)
target_include_directories(YourApp PRIVATE "${CMAKE_SOURCE_DIR}/CloverPlatform/include")
target_compile_definitions(YourApp PRIVATE CLOVERPIC_HAS_CLOVER_PLATFORM=1)
target_link_libraries(YourApp PRIVATE
    "${CMAKE_SOURCE_DIR}/CloverPlatform/target/release/libclover_abi.a"
    "-framework Security" "-framework CoreFoundation")
```

Guard the code with `#if CLOVERPIC_HAS_CLOVER_PLATFORM` so the app still builds
when the submodule or cargo is missing.

## 2. Own the window, hand over the pixels

The host creates the window and the event loop; CloverPlatform owns the UI tree,
layout, rasteriser and frame buffer.

```objc
clover_surface* surface =
    clover_surface_create_from_xaml(width, height, backingScaleFactor, xaml);
if (!surface) { /* no usable font */ }

// Every frame: returns premultiplied BGRA8 in `clover_frame`.
clover_frame frame;
clover_surface_frame(surface, &frame);
```

`frame.pixels` is valid until the next `clover_surface_frame` on that surface, so
build the `CGImage` and draw it immediately (do not retain the pointer).

## 3. Forward input, not semantics

```objc
clover_surface_pointer(surface, localX, localY, CLOVER_POINTER_DOWN, 1, modifiers);
clover_surface_key(surface, 0x09, CLOVER_KEY_DOWN, 0, false); // Tab
```

Coordinates are **logical** pixels with a top-left origin. A flipped `NSView`
already matches that, so `convertPoint:fromView:nil` is all the conversion
needed.

## 4. Give the canvas its own memory

The paint canvas is the reason this layer exists:

```objc
uint64_t handle = clover_surface_add_direct(surface, 1, 1);

// After layout, ask where the canvas landed:
clover_rect box;
clover_surface_node_bounds(surface, "canvas", &box);
clover_direct_resize(surface, handle, box.w * scale, box.h * scale);

// Canvas thread:
uint32_t* pixels; size_t stride; uint32_t height;
clover_direct_begin_write(surface, handle, &pixels, &stride, &height);
paint_into(pixels, stride, height);
clover_direct_commit(surface, handle);
```

`begin_write`/`commit` swap buffers: the UI thread keeps presenting the previous
frame until the commit, so the canvas can run at its own rate without tearing.

## 5. Guard rails found the hard way

* **Damage is logical; the rasteriser is physical.** `Surface` converts before
  handing damage to the renderer. A host that tracks its own damage must do the
  same, or hovers repaint the wrong region on Retina.
* **One damage list.** `UiTree` owns damage. If the host keeps a second one, input
  driven updates never reach the renderer.
* **Create the CloverPlatform window last.** In an AppKit app, whichever window is
  keyed last receives keyboard input; the host's own windows will otherwise keep
  the focus.
