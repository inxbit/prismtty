//! PrismTTY terminal highlighting primitives.
//!
//! This crate exposes the configuration parser, bundled profile store, style
//! model, and highlighters used by the `prismtty`, `ptty`, and `ct` binaries.

/// Command-line entry point and CLI error type.
pub mod cli;
/// Configuration loading and profile YAML parsing.
pub mod config;
/// Batch and streaming terminal output highlighters.
pub mod highlight;
pub(crate) mod profile_runtime;
/// Built-in profile definitions and profile detection helpers.
pub mod profiles;
/// Terminal style parsing and ANSI color rendering.
pub mod style;

pub use config::PrismConfig;
pub use highlight::{Highlighter, StreamingHighlighter, StyledSpan};
pub use profiles::ProfileStore;
