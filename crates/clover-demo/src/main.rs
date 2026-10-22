//! Headless demo: builds one UI out of three authoring modes and renders it.
//!
//! * the shell is assembled with the declarative builders,
//! * the toolbar is parsed from XAML,
//! * the canvas is a direct-memory surface written by another thread.
//!
//! The layout uses `Length::Fill` throughout, so nothing is hardcoded: the canvas
//! region is sized by the layout pass and the direct surface is resized to match
//! after the first frame, exactly as a real canvas widget would.

use std::io::BufWriter;
use std::sync::Arc;
use std::time::Instant;

use clover_core::{Color, Edges, Premul};
use clover_render::{DirectSurface, Surface};
use clover_ui::{node, xaml, Length, Node, SurfaceHandle};

const CANVAS_NAME: &str = "canvas";

fn main() {
    let started = Instant::now();

    // ---- 1. direct-memory canvas -------------------------------------
    // Stands in for CloverPic's paint surface: another thread owns these pixels
    // and writes them straight into memory, no UI-thread involvement.
    let canvas_handle = SurfaceHandle(1);
    let canvas = DirectSurface::new(16, 16);
    paint_canvas(&canvas);

    // ---- 2. XAML toolbar ---------------------------------------------
    let toolbar_xaml = r##"
<StackPanel Orientation="Horizontal" Spacing="6" Padding="8" Background="#25282C">
    <Button Content="Brush"  Width="72" x:Name="btnBrush"  Focusable="True"/>
    <Button Content="Eraser" Width="72" x:Name="btnEraser" Focusable="True"/>
    <Button Content="Select" Width="72" x:Name="btnSelect" Focusable="True"/>
    <Button Content="Layers" Width="72" x:Name="btnLayers" Focusable="True"/>
</StackPanel>"##;
    let toolbar = xaml::parse(toolbar_xaml, &xaml::NoImages).expect("toolbar xaml");

    // ---- 3. declarative shell ----------------------------------------
    let root = build_shell(canvas_handle, toolbar);

    let width = 900.0;
    let height = 560.0;
    let scale = 2.0;
    let mut surface =
        Surface::new(root, width, height, scale).expect("a system font is required for the demo");
    surface.register_direct(canvas_handle, Arc::clone(&canvas));

    // ---- 4. let layout size the canvas, then paint it -----------------
    surface.invalidate_all();
    surface.frame();

    let canvas_bounds = surface
        .tree()
        .named(CANVAS_NAME)
        .map(|node| node.bounds)
        .unwrap_or_default();
    let physical = (
        (canvas_bounds.w * scale).round().max(1.0) as u32,
        (canvas_bounds.h * scale).round().max(1.0) as u32,
    );
    canvas.resize(physical.0, physical.1);
    paint_canvas(&canvas);

    // ---- 5. final render ----------------------------------------------
    let render_started = Instant::now();
    surface.invalidate_all();
    let frame = surface.frame();
    let render_elapsed = render_started.elapsed();

    let out = std::path::Path::new("demo.png");
    write_png(out, &surface).expect("write demo.png");

    // Scroll the list through the same path a host uses, then render again.
    use clover_ui::{Modifiers, WheelEvent};
    surface.dispatch(clover_ui::UiEvent::Wheel(WheelEvent {
        delta: -4.0,
        position: clover_core::Point::new(90.0, 260.0),
        modifiers: Modifiers::NONE,
    }));
    surface.invalidate_all();
    let _ = surface.frame();
    let scrolled = std::path::Path::new("demo-scrolled.png");
    write_png(scrolled, &surface).expect("write demo-scrolled.png");

    let focus_order = focus_order(&surface);
    println!("CloverPlatform demo");
    println!("  nodes            : {}", surface.tree().root().count());
    println!("  frame            : {}x{} px (scale {scale})", frame.width, frame.height);
    println!("  canvas region    : {}x{} logical -> {}x{} px", canvas_bounds.w, canvas_bounds.h, physical.0, physical.1);
    println!("  raster tiles     : {}", surface.renderer_mut().last_tile_count());
    println!("  build + layout   : {:.2} ms", started.elapsed().as_secs_f32() * 1000.0);
    println!("  final render     : {:.2} ms", render_elapsed.as_secs_f32() * 1000.0);
    println!("  focus order      : {}", focus_order.join(" -> "));
    println!("  output           : {}", out.display());
    println!("  scrolled output  : {} (4 wheel notches)", scrolled.display());

    // A whole UI from one .cui file: resources + data + styles + page.
    write_pattern("examples/pattern.png");
    if let Ok(document) = xaml::UiDocument::from_path("examples/workspace.cui") {
        let mut cui_surface =
            Surface::new(node::panel(), 760.0, 480.0, 2.0).expect("system font required");
        for (key, relative) in document.resources() {
            let full = std::path::Path::new("examples").join(relative);
            cui_surface
                .renderer_mut()
                .images
                .load_png_named(key.clone(), &full)
                .expect("resource image");
        }
        let root = {
            let surface = &cui_surface;
            document
                .build(&|key: &str| surface.renderer().images.handle_for(key))
                .expect("cui build")
        };
        if let Ok(Some(overlay)) = document.build_overlay(&|key: &str| {
            cui_surface.renderer().images.handle_for(key)
        }) {
            cui_surface.set_overlay(Some(overlay));
        }
        cui_surface.set_root(root);
        cui_surface.invalidate_all();
        let _ = cui_surface.frame();
        let path = std::path::Path::new("demo-cui.png");
        write_png(path, &cui_surface).expect("write cui png");
        println!("  cui document     : {} ({} values, {} rules, {} resources)",
                 path.display(), document.data.len(), document.sheet.len(),
                 document.resources().len());
    }

    // The same page, one stylesheet each. No colour appears in the markup.
    for (name, sheet) in [("dark", DARK_THEME), ("light", LIGHT_THEME)] {
        let sheet = xaml::parse_stylesheet(sheet).expect("theme parses");
        let page = xaml::parse_with_styles(THEME_PAGE, &xaml::NoImages, &sheet).expect("themed page");
        let mut themed = Surface::new(page, 420.0, 300.0, 2.0).expect("system font required");
        themed.invalidate_all();
        let _ = themed.frame();
        let path = std::path::PathBuf::from(format!("demo-theme-{name}.png"));
        write_png(&path, &themed).expect("write themed png");
        println!("  theme {name:<11}: {}", path.display());
    }
}

/// Names of the focusable nodes in tab order.
fn focus_order(surface: &Surface) -> Vec<String> {
    let mut ids = Vec::new();
    surface.tree().root().collect_focusables(&mut ids);
    ids.iter()
        .filter_map(|id| surface.tree().node(*id))
        .map(|node| node.name.clone().unwrap_or_else(|| format!("#{}", node.id.0)))
        .collect()
}

/// The retained/declarative shell that hosts the XAML toolbar and the canvas.
fn build_shell(canvas_handle: SurfaceHandle, toolbar: Node) -> Node {
    let header = node::row()
        .height(Length::Px(40.0))
        .padding(Edges::new(14.0, 0.0, 14.0, 0.0))
        .align(clover_core::Align::Center)
        .background(Color::rgb(0x2A, 0x2D, 0x31))
        .child(node::text("CloverPlatform").font_size(15.0).text_color(Color::rgb(0xE6, 0xEA, 0xEF)))
        .child(
            node::text("0.2  •  XAML + declarative + direct-memory canvas")
                .font_size(12.0)
                .margin(Edges::new(14.0, 0.0, 0.0, 0.0))
                .text_color(Color::rgb(0x8B, 0x93, 0x9D)),
        );

    let mut recent = node::column().gap(4.0);
    for index in 0..14 {
        let active = index == 0;
        recent = recent.child(layer_row(&format!("Sketch {:02}", index + 1), active));
    }

    let rail = node::column()
        .width(Length::Px(180.0))
        .height(Length::Fill)
        .padding(Edges::all(10.0))
        .gap(8.0)
        .background(Color::rgb(0x23, 0x26, 0x29))
        .child(section_title("RECENT PROJECTS"))
        .child(
            node::column()
                .height(Length::Fill)
                .scrollable()
                .child(recent),
        )
        .child(section_title("14 projects  •  wheel to scroll"));

    let canvas_panel = node::panel()
        .size(Length::Fill, Length::Fill)
        .margin(Edges::all(20.0))
        .background(Color::rgb(0x1A, 0x1C, 0x1F))
        .border(1.0, Color::rgb(0x3A, 0x3F, 0x46))
        .child(node::direct_surface(canvas_handle).size(Length::Fill, Length::Fill).name(CANVAS_NAME));

    let body = node::row()
        .height(Length::Fill)
        .background(Color::rgb(0x1E, 0x20, 0x23))
        .child(rail)
        .child(canvas_panel);

    let status = node::row()
        .height(Length::Px(26.0))
        .padding(Edges::new(14.0, 0.0, 14.0, 0.0))
        .align(clover_core::Align::Center)
        .background(Color::rgb(0x18, 0x1A, 0x1D))
        .child(
            node::text("READY  •  brush 10 px  •  zoom 100%  •  layout is Fill-driven")
                .font_size(11.0)
                .text_color(Color::rgb(0x7F, 0x88, 0x92)),
        );

    node::column()
        .size(Length::Fill, Length::Fill)
        .background(Color::rgb(0x1E, 0x20, 0x23))
        .child(header)
        .child(toolbar)
        .child(body)
        .child(status)
}

fn section_title(label: &str) -> Node {
    node::text(label).font_size(11.0).text_color(Color::rgb(0x9A, 0xA3, 0xAD))
}

fn layer_row(label: &str, active: bool) -> Node {
    node::row()
        .height(Length::Px(28.0))
        .padding(Edges::new(8.0, 0.0, 8.0, 0.0))
        .align(clover_core::Align::Center)
        .background(if active { Color::rgb(0x2F, 0x4A, 0x6B) } else { Color::rgb(0x2A, 0x2D, 0x31) })
        .radius(4.0)
        .child(node::text(label).font_size(12.0).text_color(Color::rgb(0xD6, 0xDB, 0xE1)))
}

/// A settings card described once, entirely uncoloured: every colour, radius and
/// size comes from the stylesheet.
const THEME_PAGE: &str = r##"
<StackPanel x:Name="root" Width="Fill" Height="Fill">
  <StackPanel x:Name="titlebar" Orientation="Horizontal" Spacing="10">
    <TextBlock Text="APPEARANCE" x:Name="title"/>
  </StackPanel>
  <StackPanel x:Name="card" Spacing="12">
    <TextBlock Text="Theme" x:Name="cardTitle"/>
    <TextBlock Text="Changes apply immediately to every open window, including panels that are already floating." TextWrapping="Wrap"/>
    <StackPanel Orientation="Horizontal" Spacing="8">
      <Button Content="Dark"   x:Name="primary"/>
      <Button Content="Light"/>
      <Button Content="System"/>
    </StackPanel>
    <TextBlock Text="Colours, radii and spacing above all come from a stylesheet." x:Name="footnote"/>
  </StackPanel>
</StackPanel>
"##;

const DARK_THEME: &str = r##"
<Styles>
  <Style TargetType="TextBlock" FontSize="13" Foreground="#D7DBE0"/>
  <Style TargetType="Button" FontSize="13" Padding="16,8" CornerRadius="6"
         Background="#333A42" Foreground="#F2F5F7"/>
  <Style x:Name="title" FontSize="11" Foreground="#8B939D"/>
  <Style x:Name="root" Background="#15171A"/>
  <Style x:Name="titlebar" Padding="18,14" Background="#1E2126"/>
  <Style x:Name="card" Background="#1E2126" CornerRadius="10" Padding="18" Margin="18"/>
  <Style x:Name="cardTitle" FontSize="18" Foreground="#FFFFFF"/>
  <Style x:Name="primary" Background="#2F6FD0" BorderThickness="1" BorderBrush="#4C8DE8"/>
  <Style x:Name="footnote" FontSize="11" Foreground="#7E8792"/>
</Styles>"##;

const LIGHT_THEME: &str = r##"
<Styles>
  <Style TargetType="TextBlock" FontSize="13" Foreground="#2A2F36"/>
  <Style TargetType="Button" FontSize="13" Padding="16,8" CornerRadius="6"
         Background="#E7EAEE" Foreground="#1B1F24"/>
  <Style x:Name="title" FontSize="11" Foreground="#6B7480"/>
  <Style x:Name="root" Background="#F4F6F8"/>
  <Style x:Name="titlebar" Padding="18,14" Background="#FFFFFF"/>
  <Style x:Name="card" Background="#FFFFFF" CornerRadius="10" Padding="18" Margin="18"/>
  <Style x:Name="cardTitle" FontSize="18" Foreground="#101317"/>
  <Style x:Name="primary" Background="#2F6FD0" Foreground="#FFFFFF" BorderThickness="1" BorderBrush="#2A64BC"/>
  <Style x:Name="footnote" FontSize="11" Foreground="#7B838D"/>
</Styles>"##;

/// Writes a small pattern texture so `.cui` resources have something to load.
fn write_pattern(path: &str) {
    const SIZE: u32 = 96;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let checker = ((x / 12) + (y / 12)) % 2 == 0;
            let stripe = (x + y) % 48 < 6;
            let (r, g, b) = if stripe {
                (0x2F, 0x6F, 0xD0)
            } else if checker {
                (0x1E, 0x22, 0x27)
            } else {
                (0x18, 0x1B, 0x1F)
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    let Ok(file) = std::fs::File::create(path) else { return };
    let mut encoder = png::Encoder::new(BufWriter::new(file), SIZE, SIZE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(&rgba);
    }
}

/// Writes premultiplied pixels straight into the direct surface.
fn paint_canvas(canvas: &Arc<DirectSurface>) {
    canvas.write(|pixels, w, h| {
        let (w, h) = (w as usize, h as usize);
        // Checkerboard, like a transparent canvas.
        for y in 0..h {
            for x in 0..w {
                let checker = ((x / 16) + (y / 16)) % 2 == 0;
                let base = if checker { 0x3A } else { 0x2E };
                let c = Color::rgb(base, base, base + 4).premultiply();
                pixels[y * w + x] = c.0;
            }
        }

        // Two brush-like strokes so the canvas is obviously live content.
        let width = w as f32;
        let height = h as f32;
        for (index, colour) in [(0u32, Color::rgb(0x3D, 0x8B, 0xE0)), (1, Color::rgb(0xE0, 0x6A, 0x4A))] {
            let radius = (w as f32 * 0.013).max(4.0) - index as f32 * 2.0;
            for step in 0..220 {
                let t = step as f32 / 220.0;
                let cx = width * 0.18 + t * width * 0.62;
                let cy = height * 0.32 + (t * 6.0).sin() * height * 0.2 + index as f32 * height * 0.22;
                stamp_circle(pixels, w, h, cx, cy, radius, colour);
            }
        }
    });
}

fn stamp_circle(pixels: &mut [u32], w: usize, h: usize, cx: f32, cy: f32, radius: f32, colour: Color) {
    let x0 = (cx - radius).floor().max(0.0) as usize;
    let x1 = ((cx + radius).ceil() as usize).min(w.saturating_sub(1));
    let y0 = (cy - radius).floor().max(0.0) as usize;
    let y1 = ((cy + radius).ceil() as usize).min(h.saturating_sub(1));
    let source = colour.premultiply();
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (radius - distance).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let index = y * w + x;
            let dst = Premul(pixels[index]);
            pixels[index] = source.with_coverage((coverage * 255.0) as u8).over(dst).0;
        }
    }
}

fn write_png(path: &std::path::Path, surface: &Surface) -> Result<(), String> {
    let canvas = surface.canvas();
    let rgba = canvas.to_rgba8();
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), canvas.width, canvas.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok(())
}
