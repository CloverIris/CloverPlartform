//! Loads one `.cui` document and reports the first error, for debugging.
use clover_ui::xaml::UiDocument;

fn main() {
    let path = std::env::args().nth(1).expect("usage: load <document.cui>");
    match UiDocument::from_path(&path) {
        Ok(document) => {
            match document.build(&clover_ui::xaml::NoImages) {
                Ok(root) => println!("ok: {} nodes", root.count()),
                Err(error) => println!("build error at {}: {}", error.offset, error.message),
            }
        }
        Err(error) => println!("parse error: {error}"),
    }
}
