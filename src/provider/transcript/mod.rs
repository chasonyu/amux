//! Agent-neutral transcript blocks + renderer.
//!
//! Providers load [`TranscriptBlock`] via their own parsers (OMP:
//! [`omp::load`]); Shell draws with [`render_blocks`].

mod markdown;
pub(crate) mod omp;
mod render;
mod util;

pub use markdown::{render_markdown, MdKind, MdLine};
pub use omp::ModifiedFilesScan;
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
    User {
        text: String,
        synthetic: bool,
    },
    Assistant {
        text: String,
    },
    /// Already one-line; body discarded at parse.
    Thinking {
        summary: String,
    },
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
    Meta {
        text: String,
    },
    /// omp custom_message: typed label + markdown body.
    Custom {
        custom_type: String,
        content: String,
    },
    Spacer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Write,
    Edit,
}

impl FileOp {
    pub fn as_str(self) -> &'static str {
        match self {
            FileOp::Write => "write",
            FileOp::Edit => "edit",
        }
    }
}

/// Largest text kept per hunk; longer payloads are cut and flagged.
pub(crate) const HUNK_TEXT_MAX: usize = 32 * 1024;
/// Most recent hunks kept per file — older ones collapse into a note.
pub(crate) const HUNKS_PER_FILE_MAX: usize = 16;
/// Total hunk text kept per file.
pub(crate) const HUNK_BYTES_PER_FILE_MAX: usize = 128 * 1024;
/// Aggregated files kept per session.
pub(crate) const FILES_MAX: usize = 400;

/// One raw change captured from a file-modifying tool call. Raw text is kept
/// and diffs are rendered on demand for the focused file only, so a session
/// full of large writes costs text bytes rather than per-line allocations.
#[derive(Debug, Clone)]
pub(crate) enum FileHunk {
    /// `write` payload — full file content, no baseline.
    Content(String),
    /// `edit` with an `edits[]` array — line diff computed at render time.
    Replace { old: String, new: String },
    /// `edit` patch DSL — line-addressed ops carry no old text, so the diff
    /// lines are settled while parsing.
    Patch(Vec<DiffLine>),
}

impl FileHunk {
    /// Retained text bytes, for the per-file budget.
    pub(crate) fn bytes(&self) -> usize {
        match self {
            FileHunk::Content(s) => s.len(),
            FileHunk::Replace { old, new } => old.len() + new.len(),
            FileHunk::Patch(lines) => lines.iter().map(|l| l.text.len()).sum(),
        }
    }
}

/// One rendered diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// One file touched by successful `edit` / `write` calls in a session JSONL,
/// aggregated by path. Retained changes stay inside [`ModifiedFilesScan`] and
/// are rendered on demand, so this row stays cheap to clone.
#[derive(Debug, Clone)]
pub struct ModifiedFile {
    /// Path relative to the session cwd when it resolves under it.
    pub path: String,
    /// Number of successful modifying calls against this path.
    pub count: usize,
    /// Operation kind of the most recent call.
    pub last_op: FileOp,
    /// ISO 8601 timestamp of the most recent call (top-level `timestamp`).
    pub last_time: Option<String>,
}

/// Cut `text` to [`HUNK_TEXT_MAX`] on a char boundary, flagging the cut.
pub(crate) fn clamp_hunk_text(text: &str) -> String {
    if text.len() <= HUNK_TEXT_MAX {
        return text.to_string();
    }
    let mut end = HUNK_TEXT_MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (hunk truncated)\n", &text[..end])
}

/// Rendered diff lines kept for one file; the panel shows one screenful, so
/// anything past this is unreachable and only costs allocations.
pub(crate) const DIFF_LINES_MAX: usize = 20_000;
/// LCS table cells (`old_lines * new_lines`) above which the diff degrades to
/// delete-all + add-all instead of allocating a huge table.
const LCS_CELLS_MAX: usize = 1_000_000;

/// Render a file's retained changes into unified diff lines, oldest first.
/// Called for the focused file only — see [`FileHunk`].
pub(crate) fn render_hunks(hunks: &[FileHunk]) -> Vec<DiffLine> {
    let mut out: Vec<DiffLine> = Vec::new();
    for hunk in hunks {
        if out.len() >= DIFF_LINES_MAX {
            out.push(DiffLine {
                kind: DiffKind::Context,
                text: "… (diff truncated)".into(),
            });
            break;
        }
        match hunk {
            FileHunk::Content(new) => out.extend(new.lines().map(|l| DiffLine {
                kind: DiffKind::Add,
                text: l.to_string(),
            })),
            FileHunk::Replace { old, new } => lcs_diff(old, new, &mut out),
            FileHunk::Patch(lines) => out.extend(lines.iter().cloned()),
        }
    }
    out
}

/// Line-level diff of `old` → `new` appended to `out`. Common head/tail are
/// emitted as context without entering the table, which keeps the quadratic
/// part to the lines that actually differ.
fn lcs_diff(old: &str, new: &str, out: &mut Vec<DiffLine>) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    let push = |out: &mut Vec<DiffLine>, kind: DiffKind, lines: &[&str]| {
        out.extend(lines.iter().map(|l| DiffLine {
            kind,
            text: l.to_string(),
        }));
    };
    push(out, DiffKind::Context, &a[..head]);

    let am = &a[head..a.len() - tail];
    let bm = &b[head..b.len() - tail];
    let (n, m) = (am.len(), bm.len());
    if n == 0 || m == 0 || n.saturating_mul(m) > LCS_CELLS_MAX {
        push(out, DiffKind::Del, am);
        push(out, DiffKind::Add, bm);
    } else {
        // dp[i * (m + 1) + j] = LCS length of am[i..] and bm[j..].
        let mut dp = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * (m + 1) + j] = if am[i] == bm[j] {
                    dp[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
                };
            }
        }
        // Walk the table emitting context / del / add lines in source order.
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                out.push(DiffLine {
                    kind: DiffKind::Context,
                    text: am[i].to_string(),
                });
                i += 1;
                j += 1;
            } else if j < m && (i >= n || dp[i * (m + 1) + j + 1] > dp[(i + 1) * (m + 1) + j]) {
                out.push(DiffLine {
                    kind: DiffKind::Add,
                    text: bm[j].to_string(),
                });
                j += 1;
            } else {
                out.push(DiffLine {
                    kind: DiffKind::Del,
                    text: am[i].to_string(),
                });
                i += 1;
            }
        }
    }
    push(out, DiffKind::Context, &a[a.len() - tail..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_hunk_emits_add_del_context() {
        let d = render_hunks(&[FileHunk::Replace {
            old: "a\nb\n".into(),
            new: "a\nc\n".into(),
        }]);
        // Common head "a" as context, then the differing lines.
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].kind, DiffKind::Context);
        assert_eq!(d[0].text, "a");
        assert_eq!(d[1].kind, DiffKind::Del);
        assert_eq!(d[1].text, "b");
        assert_eq!(d[2].kind, DiffKind::Add);
        assert_eq!(d[2].text, "c");
    }

    #[test]
    fn content_hunk_is_all_added() {
        let d = render_hunks(&[FileHunk::Content("x\ny\n".into())]);
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|l| l.kind == DiffKind::Add));
        assert_eq!(d[0].text, "x");
    }

    #[test]
    fn common_tail_stays_context() {
        let d = render_hunks(&[FileHunk::Replace {
            old: "head\nold\ntail\n".into(),
            new: "head\nnew\ntail\n".into(),
        }]);
        let kinds: Vec<DiffKind> = d.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffKind::Context,
                DiffKind::Del,
                DiffKind::Add,
                DiffKind::Context
            ]
        );
        assert_eq!(d[3].text, "tail");
    }

    #[test]
    fn clamp_hunk_text_cuts_on_char_boundary() {
        let text = "中".repeat(HUNK_TEXT_MAX); // 3 bytes per char
        let cut = clamp_hunk_text(&text);
        assert!(cut.len() < text.len());
        assert!(cut.ends_with("(hunk truncated)\n"));
    }
}
