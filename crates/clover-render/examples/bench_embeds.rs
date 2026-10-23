//! Measures embedded-document rendering, single-threaded vs parallel.
//!
//! Run twice:
//!   RAYON_NUM_THREADS=1 cargo run --release -p clover-render --example bench_embeds
//!   RAYON_NUM_THREADS=8 cargo run --release -p clover-render --example bench_embeds

use std::time::Instant;

use clover_render::Surface;
use clover_ui::xaml;

const CHILDREN: usize = 8;
const ROWS: usize = 30;
const FRAMES: usize = 30;

fn child_document(index: usize) -> String {
    let mut rows = String::new();
    for row in 0..ROWS {
        rows.push_str(&format!(
            "<StackPanel Orientation=\"Horizontal\" Height=\"20\" Padding=\"6,0\">\
               <Panel Width=\"12\" Height=\"12\" Background=\"#4A9EFF\" CornerRadius=\"6\"/>\
               <TextBlock Text=\"panel {index} row {row} lorem ipsum\" Margin=\"8,0,0,0\"/>\
             </StackPanel>"
        ));
    }
    format!("<Ui><Panel Background=\"#202020\" Width=\"Fill\" Height=\"Fill\">{rows}</Panel></Ui>")
}

fn main() {
    let dir = std::env::temp_dir().join("clover-bench-embeds");
    std::fs::create_dir_all(&dir).unwrap();
    let mut content = String::new();
    for index in 0..CHILDREN {
        let path = dir.join(format!("panel{index}.cui"));
        std::fs::write(&path, child_document(index)).unwrap();
        content.push_str(&format!(
            "<Content x:Name=\"p{index}\" Source=\"{}\" Width=\"180\" Height=\"400\"/>",
            path.display()
        ));
    }

    let markup = format!(
        "<Panel Background=\"#101010\" Width=\"Fill\" Height=\"Fill\">{content}</Panel>"
    );
    let root = xaml::parse(&markup, &xaml::NoImages).unwrap();
    let mut surface = Surface::new(root, 1500.0, 500.0, 2.0).expect("system font required");
    let _ = surface.frame();

    // Force every child to re-render each frame, which is the worst case: an
    // inspector panel that updates continuously.
    let start = Instant::now();
    for _ in 0..FRAMES {
        for index in 0..CHILDREN {
            let marker = format!("panel {index} row 0 lorem ipsum");
            let _ = surface.set_label(&format!("p{index}:text"), &marker);
        }
        surface.invalidate_all();
        let _ = surface.frame();
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

    println!(
        "{CHILDREN} embedded documents, {} rayon threads: {elapsed:.2} ms/frame",
        rayon::current_num_threads()
    );
}
