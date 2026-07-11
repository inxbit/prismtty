//! PrismTTY terminal highlighting primitives.
//!
//! This crate exposes the configuration parser, bundled profile store, style
//! model, and highlighters used by the `prismtty`, `ptty`, and `ct` binaries.

#[cfg(not(unix))]
compile_error!(
    "PrismTTY targets Unix-like platforms only: it requires a PTY, termios, and POSIX signals. \
     On Windows, run it under WSL."
);

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
pub(crate) mod terminal_text;

pub use config::PrismConfig;
pub use highlight::{Highlighter, RuleMatchError, StreamingHighlighter, StyledSpan};
pub use profiles::ProfileStore;
