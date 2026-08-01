//! omp JSONL → [`TranscriptBlock`] (stub until Task 5).

use std::path::Path;

use super::TranscriptBlock;

/// Parse an omp session jsonl into neutral transcript blocks.
///
/// Currently a stub: returns empty. Task 5 ports the line-based parser.
pub fn load(_path: &Path) -> Vec<TranscriptBlock> {
    Vec::new()
}
