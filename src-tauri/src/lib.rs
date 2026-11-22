//! Xenon library crate.
//!
//! All modules are declared in `main.rs` (the binary) because Tauri's
//! `generate_context!` macro and `tauri::generate_handler!` need them in
//! the binary crate.  This library exists only to satisfy Cargo's `[lib]`
//! target; it intentionally re-exports nothing to avoid double-compiling
//! the module tree.

/// Built-in mock MCP server (`mcp_mock`). Declared here as well as in
/// `main.rs` so `tests/mcp_mock.rs` can drive it without a separate binary.
pub mod mcp_mock;