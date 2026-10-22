//! Full-frame vs damage repaint, at a 4K-ish size.
//!
//! Run with `cargo run --release --example bench_surface`.

use std::time::Instant;

use clover_core::Color;
use clover_render::Surface;
use clover_ui::{node, Length};

fn main() {
    let root = node::column()
        .background(Color::rgb(0x1A, 0x1C, 0x20))
        .child(
            node::row()
                .padding(clover_core::Edges::all(8.0))
                .gap(6.0)
                .background(Color::rgb(0x25, 0x28, 0x2C))
                .children_of((0..4).map(|index| {
                    node::button(format!("B{index}")).width(Length::Px(72.0))
                })),
        )
        .child(
            node::row()
                .height(Length::Fill)
                .padding(clover_core::Edges::all(12.0))
                .gap(12.0)
                .child(node::panel().width(Length::Px(200.0)).height(Length::Fill).background(Color::rgb(0x1D, 0x20, 0x24)))
                .child(node::panel().width(Length::Fill).height(Length::Fill).background(Color::rgb(0x16, 0x18, 0x1C))),
        );

    let mut surface = Surface::new(root, 1500.0, 850.0, 2.0).expect("system font required");
    surface.invalidate_all();
    let _ = surface.frame();

    const FRAMES: u32 = 60;
    let start = Instant::now();
    for _ in 0..FRAMES {
        surface.invalidate_all();
        let _ = surface.frame();
    }
    let full = start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

    let start = Instant::now();
    for index in 0..FRAMES * 10 {
        let x = 40.0 + (index % 4) as f32 * 80.0;
        surface.dispatch(clover_ui::UiEvent::Pointer(clover_ui::PointerEvent {
            action: clover_ui::PointerAction::Move,
            position: clover_core::Point::new(x, 24.0),
            ..Default::default()
        }));
        let _ = surface.frame();
    }
    let hover = start.elapsed().as_secs_f64() * 1000.0 / (FRAMES * 10) as f64;

    let frame = surface.frame();
    println!("CloverPlatform {}x{} px", frame.width, frame.height);
    println!("  full frame (invalidate) : {full:6.2} ms");
    println!("  hover repaint (damage)  : {hover:6.3} ms  ({:.0}x cheaper)", full / hover);
}
