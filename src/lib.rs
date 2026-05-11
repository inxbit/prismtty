pub mod cli;
pub mod config;
pub mod highlight;
pub(crate) mod profile_runtime;
pub mod profiles;
pub mod style;

pub use config::PrismConfig;
pub use highlight::{Highlighter, StreamingHighlighter, StyledSpan};
pub use profiles::ProfileStore;
