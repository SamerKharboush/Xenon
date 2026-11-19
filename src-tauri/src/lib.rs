//! Xenon library crate.
//!
//! All modules are declared in `main.rs` (the binary) because Tauri's
//! `generate_context!` macro and `tauri::generate_handler!` need them in
//! the binary crate.  This library exists only to satisfy Cargo's `[lib]`
//! target; it intentionally re-exports nothing to avoid double-compiling
//! the module tree.

/// Placeholder so the crate has at least one item.
#[allow(dead_code)]
fn _lib_placeholder() {}