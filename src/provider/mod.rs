pub mod api;
pub mod omp;
pub mod registry;
pub mod transcript;
pub mod turn_status;
pub mod watch;

#[cfg(test)]
pub mod test_support;

pub use api::{
    AgentProvider, LiveRenameAction, ModifiedFilesScanner, ProviderCapabilities, ProviderChange,
    ProviderId, ProviderSession, SessionKey, SpawnSpec, TitleSource,
};
pub use omp::OmpProvider;
pub use registry::ProviderRegistry;
pub use transcript::{
    render_blocks, DiffKind, DiffLine, FileOp, ModifiedFile, RenderedLine, RenderedSpan, SpanStyle,
    ToolKind, ToolStatus, TranscriptBlock, TranscriptRole,
};
