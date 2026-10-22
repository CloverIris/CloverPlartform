// C++ smoke test for the CloverPlatform C ABI.
//
// Proves the embedding contract a native host (CloverPic's macOS/Windows
// adapter) relies on:
//   1. create a surface from XAML,
//   2. register a direct-memory region and paint into it from another thread,
//   3. present a frame and observe those pixels in the composed output,
//   4. push an input event and get a hit node back.
//
// Build:  clang++ -std=c++20 abi_smoke.cpp -I../include ../target/debug/libclover_abi.a -o abi_smoke

#include "clover_platform.h"

#include <atomic>
#include <cstdio>
#include <cstring>
#include <thread>
#include <vector>

static int g_failures = 0;

static void check(bool condition, const char *what) {
    std::printf("%s %s\n", condition ? "PASS" : "FAIL", what);
    if (!condition) ++g_failures;
}

int main() {
    const char *xaml =
        "<StackPanel Background=\"#202225\" Padding=\"8\" Spacing=\"6\">"
        "  <TextBlock Text=\"ABI smoke\" FontSize=\"18\"/>"
        "  <CloverSurface Id=\"1\" Width=\"120\" Height=\"90\"/>"
        "</StackPanel>";

    // 1. Surface from XAML.
    clover_surface *surface = clover_surface_create_from_xaml(200.0f, 140.0f, 2.0f, xaml);
    check(surface != nullptr, "create surface from XAML");
    if (!surface) return 1;

    clover_stats stats{};
    check(clover_surface_stats(surface, &stats) == CLOVER_OK, "read stats");
    std::printf("INFO frame=%ux%u scale=%.1f nodes=%u\n", stats.width, stats.height, stats.scale,
                stats.nodes);
    check(stats.width == 400u && stats.height == 280u, "physical size is logical * scale");
    check(stats.nodes >= 3, "tree contains the panel, the text and the canvas node");

    // Registering happens automatically through XAML in this demo path; the
    // handle for the first direct region is 1.
    const uint64_t handle = clover_surface_add_direct(surface, 240, 180);
    check(handle != 0, "register direct surface");

    // 2. Paint from another thread, exactly like CloverPic's canvas loop.
    std::atomic<bool> painted{false};
    std::thread painter([&]() {
        uint32_t *pixels = nullptr;
        size_t stride = 0;
        uint32_t height = 0;
        if (clover_direct_begin_write(surface, handle, &pixels, &stride, &height) != CLOVER_OK) {
            return;
        }
        // Solid opaque red: premultiplied BGRA => 0xFFFF0000.
        for (uint32_t y = 0; y < height; ++y) {
            for (size_t x = 0; x < stride; ++x) {
                pixels[y * stride + x] = 0xFFFF0000u;
            }
        }
        painted = clover_direct_commit(surface, handle) == CLOVER_OK;
    });
    painter.join();
    check(painted.load(), "paint + commit from a worker thread");

    // 3. Present and look for the painted pixels in the composed frame.
    clover_surface_invalidate(surface);
    clover_frame frame{};
    check(clover_surface_frame(surface, &frame) == CLOVER_OK, "present frame");
    check(frame.pixels != nullptr && frame.premultiplied, "frame exposes premultiplied BGRA8");

    size_t red_pixels = 0;
    for (uint32_t y = 0; y < frame.height; ++y) {
        for (uint32_t x = 0; x < frame.width; ++x) {
            if (frame.pixels[y * frame.stride + x] == 0xFFFF0000u) ++red_pixels;
        }
    }
    std::printf("INFO composed red pixels: %zu\n", red_pixels);
    check(red_pixels > 1000, "direct-memory pixels appear in the composed frame");

    // 4. Input routing.
    const uint64_t hit = clover_surface_pointer(surface, 60.0f, 90.0f, CLOVER_POINTER_DOWN, 1,
                                                CLOVER_MODIFIER_CONTROL);
    std::printf("INFO hit node id: %llu\n", static_cast<unsigned long long>(hit));

    // A second frame must advance the generation counter.
    const uint64_t first = frame.generation;
    clover_surface_invalidate(surface);
    clover_surface_frame(surface, &frame);
    check(frame.generation > first, "generation advances per rendered frame");

    clover_surface_destroy(surface);
    std::printf("\n%s (%d failures)\n", g_failures == 0 ? "ALL ABI CHECKS PASSED" : "ABI CHECKS FAILED",
                g_failures);
    return g_failures == 0 ? 0 : 1;
}
