//! The console is embedded from `console/dist` at compile time. That
//! directory is produced by the frontend build, which must not become a
//! prerequisite for `cargo build` — a Rust-only checkout, a docs build, or
//! a contributor without Node still has to compile. When the real bundle
//! is absent we place a placeholder so the embed has something to read;
//! release builds run the frontend build first and get the real thing.

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../console/dist");
    println!("cargo:rerun-if-changed=../../console/dist");

    if dist.join("index.html").exists() {
        return;
    }
    if let Err(err) = std::fs::create_dir_all(&dist) {
        println!("cargo:warning=could not create console placeholder: {err}");
        return;
    }
    let placeholder = "<!doctype html>\n<meta charset=\"utf-8\">\n<title>caret-router console</title>\n\
        <p>The console bundle was not built into this binary. Run <code>npm ci && npm run build</code> \
        in <code>console/</code> and rebuild, or use <code>--no-default-features</code> to drop the \
        console entirely.</p>\n";
    if let Err(err) = std::fs::write(dist.join("index.html"), placeholder) {
        println!("cargo:warning=could not write console placeholder: {err}");
    }
}
