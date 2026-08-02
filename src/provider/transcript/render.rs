//! Collapsed transcript block → display lines (theme colors applied in shell).

use crate::theme::Theme;

use super::markdown::{render_markdown, MdKind, MdLine};
use super::{
    ToolKind, ToolStatus, TranscriptBlock, TranscriptRole, COLLAPSED_ITEMS, COLLAPSED_LINES,
    OUTPUT_COLLAPSED,
};

const THINKING_DISPLAY_MAX: usize = 80;
const EXPAND_HINT: &str = " [ctrl+o: Expand]";

/// Inline style kind carried per span; mirrors markdown [`MdKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStyle {
    Normal,
    Bold,
    Italic,
    Code,
    Heading,
    Link,
    Dim,
    ListBullet,
    CodeBlock,
    /// bash/eval execution border color.
    BashBorder,
    EvalBorder,
    /// omp accent (paths, highlights).
    Accent,
    /// omp custom_message label color.
    CustomLabel,
    /// Tool status icons (omp formatStatusIcon): colored per state.
    StatusOk,
    StatusErr,
    StatusPending,
}

impl SpanStyle {
    fn from_md(kind: MdKind) -> Self {
        match kind {
            MdKind::Normal => SpanStyle::Normal,
            MdKind::Bold => SpanStyle::Bold,
            MdKind::Italic => SpanStyle::Italic,
            MdKind::Code => SpanStyle::Code,
            MdKind::Heading => SpanStyle::Heading,
            MdKind::Link => SpanStyle::Link,
            MdKind::Dim => SpanStyle::Dim,
            MdKind::ListBullet => SpanStyle::ListBullet,
            MdKind::CodeBlock => SpanStyle::CodeBlock,
        }
    }
}

/// One inline-styled fragment of a rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSpan {
    pub text: String,
    pub style: SpanStyle,
}

/// One display line produced by the collapsed renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    pub role: TranscriptRole,
    pub spans: Vec<RenderedSpan>,
}

impl RenderedLine {
    fn plain(role: TranscriptRole, text: impl Into<String>) -> Self {
        Self {
            role,
            spans: vec![RenderedSpan {
                text: text.into(),
                style: SpanStyle::Normal,
            }],
        }
    }

    fn is_blank(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// Convert markdown lines into display lines, optionally prefixing a
    /// one-space left pad (User bubble).
    fn from_md(role: TranscriptRole, lines: Vec<MdLine>, pad: bool) -> Vec<Self> {
        lines
            .into_iter()
            .map(|ml| {
                let mut spans: Vec<RenderedSpan> = Vec::with_capacity(ml.spans.len() + 1);
                if pad {
                    spans.push(RenderedSpan {
                        text: " ".into(),
                        style: SpanStyle::Normal,
                    });
                }
                for s in ml.spans {
                    spans.push(RenderedSpan {
                        text: s.text,
                        style: SpanStyle::from_md(s.kind),
                    });
                }
                RenderedLine { role, spans }
            })
            .collect()
    }
}

/// Render neutral blocks to display lines for the Agent transcript preview.
pub fn render_blocks(blocks: &[TranscriptBlock], width: usize, _theme: &Theme) -> Vec<RenderedLine> {
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
            out.push(RenderedLine::plain(TranscriptRole::Meta, ""));
        }
        first_visible = false;
        out.append(&mut chunk);
    }
    out
}

fn render_one(block: &TranscriptBlock, width: usize, out: &mut Vec<RenderedLine>) {
    match block {
        TranscriptBlock::Spacer => {}
        TranscriptBlock::User { text, synthetic } => {
            // Theme has a single User style; synthetic/developer → Meta (dimmer).
            let role = if *synthetic {
                TranscriptRole::Meta
            } else {
                TranscriptRole::User
            };
            // Match omp UserMessageComponent: Markdown(text, paddingX=1, paddingY=1)
            // → blank + content(+left pad) + blank, all with user bubble bg.
            let md_width = width.saturating_sub(1).max(1);
            let body = RenderedLine::from_md(role, render_markdown(text, md_width), true);
            if body.is_empty() {
                return;
            }
            if role == TranscriptRole::User {
                out.push(RenderedLine::plain(role, ""));
            }
            out.extend(body);
            if role == TranscriptRole::User {
                out.push(RenderedLine::plain(role, ""));
            }
        }
        TranscriptBlock::Assistant { text } => {
            // Full pane width (no left pad) so table borders can use the same
            // column budget the preview actually paints into.
            let md_lines = render_markdown(text, width);
            out.extend(RenderedLine::from_md(TranscriptRole::Assistant, md_lines, false));
        }
        TranscriptBlock::Thinking { summary } => {
            let raw = if summary.trim().is_empty() {
                "Thinking"
            } else {
                summary.trim()
            };
            out.push(RenderedLine::plain(
                TranscriptRole::Thinking,
                truncate_chars(raw, THINKING_DISPLAY_MAX),
            ));
        }
        TranscriptBlock::Meta { text } => {
            out.push(RenderedLine::plain(TranscriptRole::Meta, text.clone()));
        }
        TranscriptBlock::ReadGroup { paths, status } => {
            render_read_group(paths, *status, out);
        }
        TranscriptBlock::Custom { custom_type, content } => {
            render_custom(custom_type, content, width, out);
        }
        TranscriptBlock::Tool {
            title,
            status,
            arg_preview,
            output_preview,
            kind,
            ..
        } => match kind {
            ToolKind::Bash => render_bash_eval(true, title, *status, arg_preview, output_preview, width, out),
            ToolKind::Eval => {
                render_bash_eval(false, title, *status, arg_preview, output_preview, width, out)
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
    let out_max = 4;
    let (out_visible, out_truncated, out_hidden) = tail_window(output_preview, out_max);
    let arg_truncated = arg_preview.len() > COLLAPSED_LINES;
    let any_truncated = out_truncated || arg_truncated;

    let mut header_spans = vec![
        RenderedSpan {
            text: status_icon_str(status).into(),
            style: status_span_style(status),
        },
        RenderedSpan {
            text: " ".into(),
            style: SpanStyle::Normal,
        },
        RenderedSpan {
            text: title.to_string(),
            style: SpanStyle::Normal,
        },
    ];
    if any_truncated && arg_preview.is_empty() && out_visible.is_empty() {
        header_spans.push(RenderedSpan {
            text: EXPAND_HINT.into(),
            style: SpanStyle::Dim,
        });
    }
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        spans: header_spans,
    });

    let arg_show = arg_preview.len().min(COLLAPSED_LINES);
    let has_output = !out_visible.is_empty() || out_truncated;
    for i in 0..arg_show {
        let last_arg = i + 1 == arg_show;
        let last_overall = last_arg && !has_output;
        let prefix = if last_overall { "└─ " } else { "├─ " };
        let mut spans = vec![
            RenderedSpan {
                text: prefix.into(),
                style: SpanStyle::Dim,
            },
            RenderedSpan {
                text: arg_preview[i].clone(),
                style: SpanStyle::Normal,
            },
        ];
        if last_overall && any_truncated {
            spans.push(RenderedSpan {
                text: EXPAND_HINT.into(),
                style: SpanStyle::Dim,
            });
        }
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans,
        });
    }

    if out_truncated {
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans: vec![RenderedSpan {
                text: format!("└─ … {} earlier lines", out_hidden).into(),
                style: SpanStyle::Dim,
            }],
        });
    }
    for (i, line) in out_visible.iter().enumerate() {
        let last = i + 1 == out_visible.len();
        let prefix = if last { "└─ " } else { "├─ " };
        let mut spans = vec![
            RenderedSpan {
                text: prefix.into(),
                style: SpanStyle::Dim,
            },
            RenderedSpan {
                text: line.to_string(),
                style: SpanStyle::Normal,
            },
        ];
        if last && out_truncated {
            spans.push(RenderedSpan {
                text: EXPAND_HINT.into(),
                style: SpanStyle::Dim,
            });
        }
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans,
        });
    }
}

fn render_read_group(paths: &[String], status: ToolStatus, out: &mut Vec<RenderedLine>) {
    let n = paths.len();
    let truncated = n > COLLAPSED_ITEMS;
    let show = n.min(COLLAPSED_ITEMS);
    let hidden = n.saturating_sub(show);
    let mut header_spans = vec![
        RenderedSpan {
            text: status_icon_str(status).into(),
            style: status_span_style(status),
        },
        RenderedSpan {
            text: " Read ".into(),
            style: SpanStyle::Normal,
        },
        RenderedSpan {
            text: format!("· {n} files").into(),
            style: SpanStyle::Dim,
        },
    ];
    if truncated && show == 0 {
        header_spans.push(RenderedSpan {
            text: EXPAND_HINT.into(),
            style: SpanStyle::Dim,
        });
    }
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        spans: header_spans,
    });
    for i in 0..show {
        let last = i + 1 == show;
        let prefix = if last { "└─ " } else { "├─ " };
        let mut spans = vec![
            RenderedSpan {
                text: prefix.into(),
                style: SpanStyle::Dim,
            },
            RenderedSpan {
                text: paths[i].clone(),
                style: SpanStyle::Accent,
            },
        ];
        if last && truncated {
            spans.push(RenderedSpan {
                text: format!(" … {} more", hidden).into(),
                style: SpanStyle::Dim,
            });
        }
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans,
        });
    }
}

fn render_bash_eval(
    is_bash: bool,
    title: &str,
    status: ToolStatus,
    arg_preview: &[String],
    output_preview: &[String],
    width: usize,
    out: &mut Vec<RenderedLine>,
) {
    let border = if is_bash {
        SpanStyle::BashBorder
    } else {
        SpanStyle::EvalBorder
    };
    let rule = "─".repeat(width);
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        spans: vec![RenderedSpan {
            text: rule.clone(),
            style: border,
        }],
    });

    let cmd = arg_preview
        .first()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(title);
    let header_text = if is_bash {
        format!("$ {cmd}")
    } else {
        ">>>".to_string()
    };
    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        spans: vec![RenderedSpan {
            text: header_text,
            style: SpanStyle::Normal,
        }],
    });

    let (out_visible, out_truncated, out_hidden) = tail_window(output_preview, OUTPUT_COLLAPSED);
    if out_truncated {
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans: vec![RenderedSpan {
                text: format!("└─ … {} earlier lines", out_hidden).into(),
                style: SpanStyle::Dim,
            }],
        });
    }
    for (i, line) in out_visible.iter().enumerate() {
        let last = i + 1 == out_visible.len();
        let prefix = if last { "└─ " } else { "├─ " };
        let mut spans = vec![
            RenderedSpan {
                text: prefix.into(),
                style: SpanStyle::Dim,
            },
            RenderedSpan {
                text: line.to_string(),
                style: SpanStyle::Normal,
            },
        ];
        if last && out_truncated {
            spans.push(RenderedSpan {
                text: EXPAND_HINT.into(),
                style: SpanStyle::Dim,
            });
        }
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans,
        });
    }
    if out_truncated && out_visible.is_empty() {
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans: vec![RenderedSpan {
                text: format!("└─ …{EXPAND_HINT}").into(),
                style: SpanStyle::Dim,
            }],
        });
    }

    // Stats footer: omp `[Wall: … | Exit: N]`; we only have status → Exit 0/1.
    if status != ToolStatus::Pending {
        let exit = if status == ToolStatus::Ok { 0 } else { 1 };
        out.push(RenderedLine {
            role: TranscriptRole::Tool,
            spans: vec![RenderedSpan {
                text: format!("[Exit: {exit}]").into(),
                style: SpanStyle::Dim,
            }],
        });
    }

    out.push(RenderedLine {
        role: TranscriptRole::Tool,
        spans: vec![RenderedSpan {
            text: rule,
            style: border,
        }],
    });
}

fn render_custom(custom_type: &str, content: &str, width: usize, out: &mut Vec<RenderedLine>) {
    let md_width = width.saturating_sub(1).max(1);
    out.push(RenderedLine {
        role: TranscriptRole::Custom,
        spans: vec![RenderedSpan {
            text: format!("[{custom_type}]").into(),
            style: SpanStyle::CustomLabel,
        }],
    });
    if !content.trim().is_empty() {
        let md_lines = render_markdown(content, md_width);
        out.extend(RenderedLine::from_md(TranscriptRole::Custom, md_lines, true));
    }
}

fn status_icon_str(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "✔",
        ToolStatus::Error => "✘",
        ToolStatus::Pending => "⏳",
    }
}

fn status_span_style(status: ToolStatus) -> SpanStyle {
    match status {
        ToolStatus::Ok => SpanStyle::StatusOk,
        ToolStatus::Error => SpanStyle::StatusErr,
        ToolStatus::Pending => SpanStyle::StatusPending,
    }
}

/// Tail window: last `max` non-empty lines, plus the hidden count if truncated.
fn tail_window(lines: &[String], max: usize) -> (Vec<&str>, bool, usize) {
    let max = max.max(1);
    if lines.len() <= max {
        return (lines.iter().map(|s| s.as_str()).collect(), false, 0);
    }
    let start = lines.len() - max;
    let visible: Vec<&str> = lines[start..].iter().map(|s| s.as_str()).collect();
    (visible, true, start)
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
    use std::path::PathBuf;

    fn line_text(l: &RenderedLine) -> String {
        l.spans.iter().map(|s| s.text.as_str()).collect()
    }

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
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
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
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("hi")));
        assert!(texts.iter().any(|t| t.contains("hello")));
        // Exactly one empty Meta between the two visible blocks.
        let blanks: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.role == TranscriptRole::Meta && l.is_blank())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(blanks.len(), 1);
    }

    #[test]
    fn user_bubble_has_vertical_padding_like_omp() {
        let theme = Theme::dark();
        let blocks = vec![TranscriptBlock::User {
            text: "hi".into(),
            synthetic: false,
        }];
        let lines = render_blocks(&blocks, 40, &theme);
        let user: Vec<&RenderedLine> = lines
            .iter()
            .filter(|l| l.role == TranscriptRole::User)
            .collect();
        // paddingY=1 → blank + content + blank
        assert_eq!(user.len(), 3, "{:?}", user.iter().map(|l| line_text(l)).collect::<Vec<_>>());
        assert!(user[0].is_blank());
        assert!(line_text(user[1]).contains("hi"));
        assert!(user[2].is_blank());
    }

    #[test]
    fn synthetic_user_uses_meta_role() {
        let theme = Theme::dark();
        let blocks = vec![TranscriptBlock::User {
            text: "system note".into(),
            synthetic: true,
        }];
        let lines = render_blocks(&blocks, 40, &theme);
        assert!(lines.iter().any(|l| l.role == TranscriptRole::Meta));
        assert!(!lines.iter().any(|l| l.role == TranscriptRole::User));
    }

    #[test]
    fn bash_truncated_output_has_tree_prefix() {
        let theme = Theme::dark();
        let blocks = vec![TranscriptBlock::Tool {
            name: "bash".into(),
            title: "bash".into(),
            status: ToolStatus::Ok,
            arg_preview: vec!["ls".into()],
            output_preview: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            kind: ToolKind::Bash,
        }];
        let lines = render_blocks(&blocks, 60, &theme);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("$ ls"));
        assert!(joined.contains("└─") || joined.contains("├─"));
        assert!(joined.contains(EXPAND_HINT));
    }

    #[test]
    fn assistant_bold_is_styled_not_stripped() {
        let theme = Theme::dark();
        let blocks = vec![TranscriptBlock::Assistant {
            text: "**bold** text".into(),
        }];
        let lines = render_blocks(&blocks, 60, &theme);
        let bold_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.style == SpanStyle::Bold);
        assert!(bold_span.is_some());
        assert_eq!(bold_span.unwrap().text, "bold");
    }

    #[test]
    fn assistant_code_is_styled_not_stripped() {
        let theme = Theme::dark();
        let blocks = vec![TranscriptBlock::Assistant {
            text: "run `cargo` now".into(),
        }];
        let lines = render_blocks(&blocks, 60, &theme);
        let code_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.style == SpanStyle::Code);
        assert!(code_span.is_some());
        assert_eq!(code_span.unwrap().text, "cargo");
    }

    /// Manual: dump first 40 render lines from a real ~/.omp session (skip if none).
    #[test]
    #[ignore]
    fn dump_real_omp_session_preview() {
        use super::super::load;

        let root = dirs_home_omp_sessions();
        let Some(path) = find_newest_jsonl(&root) else {
            eprintln!("no jsonl under {}; skip dump", root.display());
            return;
        };
        let theme = Theme::dark();
        let blocks = load("omp", &path);
        let lines = render_blocks(&blocks, 80, &theme);
        println!(
            "=== {} ({} blocks, {} lines) ===",
            path.display(),
            blocks.len(),
            lines.len()
        );
        for (i, l) in lines.iter().take(40).enumerate() {
            let txt: String = l.spans.iter().map(|s| s.text.as_str()).collect();
            println!("{:02} [{:?}] {}", i, l.role, txt);
        }
    }

    fn dirs_home_omp_sessions() -> PathBuf {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join(".omp/agent/sessions")
    }

    fn find_newest_jsonl(root: &std::path::Path) -> Option<PathBuf> {
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let mtime = ent
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                    newest = Some((mtime, p));
                }
            }
        }
        newest.map(|(_, p)| p)
    }
}
