//! omp JSONL → [`TranscriptBlock`].

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::provider::omp::skip_title_prefix;

use super::util::{content_to_text, format_primary_arg, one_line, read_path_arg};
use super::{
    ToolKind, ToolStatus, TranscriptBlock, COLLAPSED_LINES, OUTPUT_COLLAPSED, TRUNCATE_LINE,
    TRUNCATE_TITLE,
};

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BLOCKS: usize = 4000;
const THINKING_SUMMARY_MAX: usize = 160;

#[derive(Clone, Copy)]
enum PendingSlot {
    Tool(usize),
    ReadGroup(usize),
}

/// Parse an omp session jsonl into neutral transcript blocks.
pub fn load(path: &Path) -> Vec<TranscriptBlock> {
    let Ok(mut f) = File::open(path) else {
        return vec![TranscriptBlock::Meta {
            text: format!("(cannot open {})", path.display()),
        }];
    };
    if skip_title_prefix(&mut f).is_err() {
        let _ = f.seek(SeekFrom::Start(0));
    }

    let mut limited = f.take(MAX_READ_BYTES);
    let mut buf = String::new();
    let _ = limited.read_to_string(&mut buf);

    let mut out = Vec::new();
    let mut pending: HashMap<String, PendingSlot> = HashMap::new();
    let mut truncated = false;

    for raw in buf.lines() {
        if out.len() >= MAX_BLOCKS {
            truncated = true;
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(kind) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        match kind {
            "message" => append_message(&v, &mut out, &mut pending),
            "custom_message" => append_custom_message(&v, &mut out),
            "compaction" => {
                let summary = v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("compaction");
                push(
                    &mut out,
                    TranscriptBlock::Meta {
                        text: format!("─── compacted · {} ───", one_line(summary, TRUNCATE_TITLE)),
                    },
                );
            }
            "branch_summary" => {
                let summary = v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("branch");
                push(
                    &mut out,
                    TranscriptBlock::Meta {
                        text: format!("─── branch · {} ───", one_line(summary, TRUNCATE_TITLE)),
                    },
                );
            }
            _ => {}
        }
    }

    if truncated {
        push(
            &mut out,
            TranscriptBlock::Meta {
                text: "… (transcript truncated)".into(),
            },
        );
    }

    if out.is_empty() {
        out.push(TranscriptBlock::Meta {
            text: "(no messages yet)".into(),
        });
    }
    out
}

fn push(out: &mut Vec<TranscriptBlock>, block: TranscriptBlock) {
    if out.len() < MAX_BLOCKS {
        out.push(block);
    }
}

fn append_message(
    v: &Value,
    out: &mut Vec<TranscriptBlock>,
    pending: &mut HashMap<String, PendingSlot>,
) {
    let Some(message) = v.get("message") else {
        return;
    };
    let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
    match role {
        "user" | "developer" => {
            let text = content_to_text(message.get("content"));
            if text.trim().is_empty() {
                return;
            }
            let synthetic = role == "developer"
                || message
                    .get("synthetic")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);
            push(
                out,
                TranscriptBlock::User {
                    text: text.trim_end().to_string(),
                    synthetic,
                },
            );
        }
        "assistant" => append_assistant(message, out, pending),
        "toolResult" => apply_tool_result(message, out, pending),
        _ => {}
    }
}

fn append_assistant(
    message: &Value,
    out: &mut Vec<TranscriptBlock>,
    pending: &mut HashMap<String, PendingSlot>,
) {
    let parts = match message.get("content") {
        Some(Value::String(s)) => {
            let t = s.trim_end();
            if !t.is_empty() {
                push(
                    out,
                    TranscriptBlock::Assistant {
                        text: t.to_string(),
                    },
                );
            }
            return;
        }
        Some(Value::Array(parts)) => parts.as_slice(),
        _ => return,
    };

    let (before, tool_calls, after_by_id) = split_assistant_tool_timeline(parts);
    emit_content_parts(&before, out);

    let mut open_read_group: Option<usize> = None;

    for (id, name, args) in &tool_calls {
        let kind = tool_kind(name);
        if kind == ToolKind::Read {
            let path = read_path_arg(args.as_ref())
                .or_else(|| {
                    let a = format_primary_arg(args.as_ref());
                    if a.is_empty() {
                        None
                    } else {
                        Some(a)
                    }
                })
                .unwrap_or_else(|| name.clone());
            match open_read_group {
                Some(idx) => {
                    if let Some(TranscriptBlock::ReadGroup { paths, .. }) = out.get_mut(idx) {
                        paths.push(path);
                    }
                    if !id.is_empty() {
                        pending.insert(id.clone(), PendingSlot::ReadGroup(idx));
                    }
                }
                None => {
                    let idx = out.len();
                    push(
                        out,
                        TranscriptBlock::ReadGroup {
                            paths: vec![path],
                            status: ToolStatus::Pending,
                        },
                    );
                    open_read_group = Some(idx);
                    if !id.is_empty() {
                        pending.insert(id.clone(), PendingSlot::ReadGroup(idx));
                    }
                }
            }
        } else {
            open_read_group = None;
            let primary = format_primary_arg(args.as_ref());
            let title = if primary.is_empty() {
                name.clone()
            } else {
                one_line(&format!("{name}({primary})"), TRUNCATE_TITLE)
            };
            let arg_preview = if primary.is_empty() {
                Vec::new()
            } else {
                vec![primary]
            };
            let idx = out.len();
            push(
                out,
                TranscriptBlock::Tool {
                    name: name.clone(),
                    title,
                    status: ToolStatus::Pending,
                    arg_preview,
                    output_preview: Vec::new(),
                    kind,
                },
            );
            if !id.is_empty() {
                pending.insert(id.clone(), PendingSlot::Tool(idx));
            }
        }

        if let Some(after) = after_by_id.get(id.as_str()) {
            if content_parts_visible(after) {
                open_read_group = None;
            }
            emit_content_parts(after, out);
        }
    }
}

fn split_assistant_tool_timeline(
    parts: &[Value],
) -> (
    Vec<Value>,
    Vec<(String, String, Option<Value>)>,
    HashMap<String, Vec<Value>>,
) {
    let mut before = Vec::new();
    let mut tool_calls = Vec::new();
    let mut after_by_id: HashMap<String, Vec<Value>> = HashMap::new();
    let mut pending_after: Vec<Value> = Vec::new();
    let mut last_tool_id: Option<String> = None;
    let mut saw_tool = false;

    let flush_after = |last: &Option<String>, pending: &mut Vec<Value>, map: &mut HashMap<String, Vec<Value>>| {
        if let Some(id) = last {
            if !pending.is_empty() {
                map.insert(id.clone(), std::mem::take(pending));
            }
        } else {
            pending.clear();
        }
    };

    for part in parts {
        let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ptype == "toolCall" {
            flush_after(&last_tool_id, &mut pending_after, &mut after_by_id);
            saw_tool = true;
            let id = part
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let name = part
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("tool")
                .to_string();
            let args = part
                .get("arguments")
                .or_else(|| part.get("args"))
                .cloned();
            last_tool_id = Some(id.clone());
            tool_calls.push((id, name, args));
            continue;
        }
        if saw_tool {
            pending_after.push(part.clone());
        } else {
            before.push(part.clone());
        }
    }
    flush_after(&last_tool_id, &mut pending_after, &mut after_by_id);

    (before, tool_calls, after_by_id)
}

fn content_parts_visible(parts: &[Value]) -> bool {
    for part in parts {
        match part.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => {
                let t = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !t.trim().is_empty() {
                    return true;
                }
            }
            "thinking" => {
                let t = part
                    .get("thinking")
                    .or_else(|| part.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if !t.trim().is_empty() {
                    return true;
                }
            }
            "image" => return true,
            _ => {}
        }
    }
    false
}

fn emit_content_parts(parts: &[Value], out: &mut Vec<TranscriptBlock>) {
    for part in parts {
        let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ptype {
            "text" => {
                let t = part
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .trim_end();
                if !t.is_empty() {
                    push(
                        out,
                        TranscriptBlock::Assistant {
                            text: t.to_string(),
                        },
                    );
                }
            }
            "thinking" => {
                let t = part
                    .get("thinking")
                    .or_else(|| part.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .trim();
                if !t.is_empty() {
                    push(
                        out,
                        TranscriptBlock::Thinking {
                            summary: one_line(t, THINKING_SUMMARY_MAX),
                        },
                    );
                }
            }
            "image" => {
                push(
                    out,
                    TranscriptBlock::Assistant {
                        text: "[image]".into(),
                    },
                );
            }
            _ => {}
        }
    }
}

fn tool_kind(name: &str) -> ToolKind {
    let lower = name.to_ascii_lowercase();
    if lower == "read" {
        ToolKind::Read
    } else if lower == "bash" || lower.contains("bash") {
        ToolKind::Bash
    } else if lower == "eval" || lower.contains("eval") {
        ToolKind::Eval
    } else {
        ToolKind::Default
    }
}

fn apply_tool_result(
    message: &Value,
    out: &mut Vec<TranscriptBlock>,
    pending: &mut HashMap<String, PendingSlot>,
) {
    let id = message
        .get("toolCallId")
        .or_else(|| message.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let ok = message
        .get("isError")
        .and_then(|x| x.as_bool())
        .map(|e| !e)
        .unwrap_or(true);
    let status = if ok {
        ToolStatus::Ok
    } else {
        ToolStatus::Error
    };
    let body = content_to_text(message.get("content"));
    let preview = preview_lines(&body, OUTPUT_COLLAPSED.max(COLLAPSED_LINES));

    if !id.is_empty() {
        if let Some(slot) = pending.remove(id) {
            match slot {
                PendingSlot::Tool(idx) => {
                    if let Some(TranscriptBlock::Tool {
                        status: st,
                        output_preview,
                        ..
                    }) = out.get_mut(idx)
                    {
                        *st = status;
                        *output_preview = preview;
                    }
                    return;
                }
                PendingSlot::ReadGroup(idx) => {
                    if let Some(TranscriptBlock::ReadGroup {
                        status: st,
                        ..
                    }) = out.get_mut(idx)
                    {
                        *st = merge_status(*st, status);
                    }
                    return;
                }
            }
        }
    }

    // Orphan tool result — still surface a compact tool card.
    let tool_name = message
        .get("toolName")
        .and_then(|n| n.as_str())
        .unwrap_or("tool")
        .to_string();
    push(
        out,
        TranscriptBlock::Tool {
            name: tool_name.clone(),
            title: tool_name,
            status,
            arg_preview: Vec::new(),
            output_preview: preview,
            kind: ToolKind::Default,
        },
    );
}

fn merge_status(a: ToolStatus, b: ToolStatus) -> ToolStatus {
    use ToolStatus::*;
    match (a, b) {
        (Error, _) | (_, Error) => Error,
        (Pending, _) | (_, Pending) => Pending,
        (Ok, Ok) => Ok,
    }
}

fn preview_lines(body: &str, max: usize) -> Vec<String> {
    body.lines()
        .filter(|l| !l.is_empty())
        .take(max)
        .map(|l| one_line(l, TRUNCATE_LINE))
        .collect()
}

fn append_custom_message(v: &Value, out: &mut Vec<TranscriptBlock>) {
    if v.get("display").and_then(|d| d.as_bool()) == Some(false) {
        return;
    }
    let ctype = v
        .get("customType")
        .and_then(|t| t.as_str())
        .unwrap_or("custom");
    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
    push(
        out,
        TranscriptBlock::Custom {
            custom_type: ctype.to_string(),
            content: content.trim_end().to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_user_assistant_tool() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/provider/transcript/fixtures/sample_turn.jsonl");
        let blocks = load(&path);
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, TranscriptBlock::User { synthetic: false, .. })),
            "expected User block, got {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, TranscriptBlock::Assistant { .. })),
            "expected Assistant block, got {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, TranscriptBlock::Thinking { .. })),
            "expected Thinking block, got {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, TranscriptBlock::Tool { .. })),
            "expected Tool block, got {blocks:?}"
        );
        // toolResult should have merged into the pending Tool
        let tool = blocks
            .iter()
            .find_map(|b| match b {
                TranscriptBlock::Tool {
                    name,
                    status,
                    output_preview,
                    kind,
                    ..
                } => Some((name.as_str(), *status, output_preview, *kind)),
                _ => None,
            })
            .expect("tool");
        assert_eq!(tool.0, "bash");
        assert_eq!(tool.1, ToolStatus::Ok);
        assert_eq!(tool.3, ToolKind::Bash);
        assert!(!tool.2.is_empty());
    }
}
