#[path = "../js_repl.rs"]
mod js_repl;

/// Run the dedicated stdio transport without initializing the Dinosaur GUI.
fn main() {
    if let Err(error) = js_repl::serve_stdio() {
        eprintln!("Dinosaur JavaScript REPL: {error:#}");
        std::process::exit(1);
    }
}
