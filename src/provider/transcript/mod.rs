//! Agent-neutral transcript blocks + provider dispatch.
//!
//! Shell loads [`TranscriptBlock`] via [`load`] and draws with [`render`].

mod markdown;
mod omp;
mod render;
mod util;

use std::path::Path;

pub use markdown::{render_markdown, MdKind, MdLine};
pub use render::{render_blocks, RenderedLine, RenderedSpan, SpanStyle};

pub const COLLAPSED_LINES: usize = 3;
pub const OUTPUT_COLLAPSED: usize = 3;
pub const COLLAPSED_ITEMS: usize = 8;
pub const TRUNCATE_LINE: usize = 110;
pub const TRUNCATE_TITLE: usize = 60;
pub const TRUNCATE_ARG: usize = 100;

/// Display role used when mapping rendered lines to theme colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
    Tool,
    Thinking,
    Meta,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Ok,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Default,
    Read,
    Bash,
    Eval,
}

#[derive(Debug, Clone)]
pub enum TranscriptBlock {
    User { text: String, synthetic: bool },
    Assistant { text: String },
    /// Already one-line; body discarded at parse.
    Thinking { summary: String },
    Tool {
        name: String,
        title: String,
        status: ToolStatus,
        arg_preview: Vec<String>,
        output_preview: Vec<String>,
        kind: ToolKind,
    },
    ReadGroup {
        paths: Vec<String>,
        status: ToolStatus,
    },
    /// Compaction divider, unknown-provider notice, etc.
    Meta { text: String },
    /// omp custom_message: typed label + markdown body.
    Custom { custom_type: String, content: String },
    Spacer,
}

/// Load neutral transcript blocks for `provider` from `path`.
pub fn load(provider: &str, path: &Path) -> Vec<TranscriptBlock> {
    match provider {
        "omp" => omp::load(path),
        other => vec![TranscriptBlock::Meta {
            text: format!("(no transcript preview for provider `{other}`)"),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn unknown_provider_placeholder() {
        let b = load("other", Path::new("/nope"));
        assert!(matches!(&b[0], TranscriptBlock::Meta { text } if text.contains("other")));
    }
}
