//! Collapsed transcript block → display lines (theme colors applied in shell).

use crate::theme::Theme;

use super::legacy::TranscriptRole;
use super::markdown::render_markdown;
use super::{
    ToolKind, ToolStatus, TranscriptBlock, COLLAPSED_ITEMS, COLLAPSED_LINES, OUTPUT_COLLAPSED,
};

const THINKING_DISPLAY_MAX: usize = 80;
const EXPAND_HINT: &str = " (ctrl+o: Expand)";

/// One display line produced by the collapsed renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    pub role: TranscriptRole,
    pub text: String,
}

/// Render neutral blocks to plain lines for the Agent transcript preview.
///
/// Colors are applied later by role in the shell; `theme` is reserved for
/// future inline styling.
pub fn render_blocks(blocks: &[TranscriptBlock], width: usize, theme: &Theme) -> Vec<RenderedLine> {
    let _ = theme;
    let width = width.max(1);
    let mut out = Vec::new();
    let mut first_visible = true;

    for block in blocks {
        if matches!(block, TranscriptBlock::Spacer) {
            continue;
        }
        let mut chunk = Vec::new();
        render_one(block, width, &mut chunk);
        if chunk.is_empty() {
            continue;
        }
        if !first_visible {
            out.push(RenderedLine {
                role: TranscriptRole::Meta,
                text: String::new(),
            });
        }
        first_visible = false;
        out.append(&mut chunk);
    }
    out
}

fn render_one(block: &TranscriptBlock, width: usize, out: &mut Vec<RenderedLine>) {
    match block {
        TranscriptBlock::Spacer => {}
        TranscriptBlock::User { text, .. } => {
            let md_width = width.saturating_sub(1).max(1);
            for line in render_markdown(text, md_width) {
                out.push(RenderedLine {
                    role: TranscriptRole::User,
                    text: format!(" {}", line.text),
                });
            }
        }
        TranscriptBlock::Assistant { text } => {
            let md_width = width.saturating_sub(1).max(1);
            for line in render_markdown(text, md_width) {
                out.push(RenderedLine {
                    role: TranscriptRole::Assistant,
                    text: format!(" {}", line.text),
                });
            }
        }
        TranscriptBlock::Thinking { summary } => {
            let raw = if summary.trim().is_empty() {
                "Thinking"
            } else {
                summary.trim()
            };
            out.push(RenderedLine {
                role: TranscriptRole::Thinking,
                text: truncate_chars(raw, THINKING_DISPLAY_MAX),
            });
        }
        TranscriptBlock::Meta { text } => {
            out.push(RenderedLine {
                role: TranscriptRole::Meta,
                text: text.clone(),
            });
        }
        TranscriptBlock::ReadGroup { paths, status } => {
            render_read_group(paths, *status, out);
        }
        TranscriptBlock::Tool {
            title,
            status,
            arg_preview,
            output_preview,
            kind,
            ..
        } => match kind {
            ToolKind::Bash => render_bash_eval(true, title, arg_preview, output_preview, width, out),
            ToolKind::Eval => {
                render_bash_eval(false, title, arg_preview, output_preview, width, out)
            }
            ToolKind::Default | ToolKind::Read => {
                render_default_tool(title, *status, arg_preview, output_preview, out)
            }
        },
    }
}

fn render_default_tool(
    title: &str,
    status: ToolStatus,
    arg_preview: &[String],
    output_preview: &[String],
    out: &mut Vec<RenderedLine>,
) {
    let mut truncated = false;
    let mut tree: Vec<&str> = arg_preview.iter().map(|s| s.as_str()).collect();
    if output_preview.len() > OUTPUT_COLLAPSED {
        truncated = true;
    }
    for line in output_preview.iter().take(OUTPUT_COLLAPSED) {
        tree.push(line.as_str());
    }
    if tree.len() > COLLAPSED_LINES {
        truncated = true;
    }

    let mut header = format!("{} {}", status_icon(status), title);
    if truncated && tree.is_empty() {
        header.push_str(EXPAND_HINT);
    }
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        text: header,
    });

    let show = tree.len().min(COLLAPSED_LINES);
    for i in 0..show {
        let prefix = if i + 1 == show { "└─ " } else { "├─ " };
        let mut text = format!("{prefix}{}", tree[i]);
        if truncated && i + 1 == show {
            text.push_str(EXPAND_HINT);
        }
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            text,
        });
    }
}

fn render_read_group(paths: &[String], status: ToolStatus, out: &mut Vec<RenderedLine>) {
    let n = paths.len();
    let truncated = n > COLLAPSED_ITEMS;
    let mut header = format!("{} Read · {n} files", status_icon(status));
    let show = n.min(COLLAPSED_ITEMS);
    if truncated && show == 0 {
        header.push_str(EXPAND_HINT);
    }
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        text: header,
    });
    for i in 0..show {
        let prefix = if i + 1 == show { "└─ " } else { "├─ " };
        let mut text = format!("{prefix}{}", paths[i]);
        if truncated && i + 1 == show {
            text.push_str(EXPAND_HINT);
        }
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            text,
        });
    }
}

fn render_bash_eval(
    is_bash: bool,
    title: &str,
    arg_preview: &[String],
    output_preview: &[String],
    width: usize,
    out: &mut Vec<RenderedLine>,
) {
    let rule = "─".repeat(width);
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        text: rule.clone(),
    });

    let cmd = arg_preview
        .first()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(title);
    let header = if is_bash {
        format!("$ {cmd}")
    } else {
        ">>>".to_string()
    };
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        text: header,
    });

    let mut truncated = output_preview.len() > OUTPUT_COLLAPSED;
    let show_out = output_preview.len().min(OUTPUT_COLLAPSED).min(COLLAPSED_LINES);
    for line in output_preview.iter().take(show_out) {
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            text: line.clone(),
        });
    }
    if output_preview.len() > show_out {
        truncated = true;
    }
    if truncated {
        if let Some(last) = out.last_mut() {
            if !last.text.contains(EXPAND_HINT) {
                last.text.push_str(EXPAND_HINT);
            }
        }
    }

    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        text: rule,
    });
}

fn status_icon(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "✔",
        ToolStatus::Error => "✘",
        ToolStatus::Pending => "⏳",
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn tool_card_has_tree_prefix() {
        let theme = Theme::dark();
        let blocks = vec![TranscriptBlock::Tool {
            name: "read".into(),
            title: "Read: foo.rs".into(),
            status: ToolStatus::Ok,
            arg_preview: vec!["foo.rs".into()],
            output_preview: vec!["fn main() {}".into()],
            kind: ToolKind::Default,
        }];
        let lines = render_blocks(&blocks, 60, &theme);
        let joined = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Read"));
        assert!(joined.contains("└─") || joined.contains("├─"));
    }

    #[test]
    fn inter_block_single_blank() {
        let theme = Theme::dark();
        let blocks = vec![
            TranscriptBlock::User {
                text: "hi".into(),
                synthetic: false,
            },
            TranscriptBlock::Assistant {
                text: "hello".into(),
            },
        ];
        let lines = render_blocks(&blocks, 40, &theme);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("hi")));
        assert!(texts.iter().any(|t| t.contains("hello")));
        // Exactly one empty Meta between the two visible blocks.
        let blanks: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.role == TranscriptRole::Meta && l.text.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(blanks.len(), 1);
    }
}
