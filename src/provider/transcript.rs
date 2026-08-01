//! Read-only omp JSONL transcript for sessions without a live PTY.
//! Visual rules approximate omp interactive transcript (ui-helpers / history-format).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::omp::skip_title_prefix;

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINES_OUT: usize = 4000;
const TOOL_ARG_MAX: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
    Tool,
    Thinking,
    Meta,
}

#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub role: TranscriptRole,
    pub text: String,
}

/// Load display lines from an omp session jsonl (newest content toward the end).
pub fn load_transcript(path: &Path) -> Vec<TranscriptLine> {
    let Ok(mut f) = File::open(path) else {
        return vec![TranscriptLine {
            role: TranscriptRole::Meta,
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
    let mut pending_tools: HashMap<String, usize> = HashMap::new();

    for raw in buf.lines() {
        if out.len() >= MAX_LINES_OUT {
            out.push(TranscriptLine {
                role: TranscriptRole::Meta,
                text: "… (transcript truncated)".into(),
            });
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(kind) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        match kind {
            "message" => append_message(&v, &mut out, &mut pending_tools),
            "custom_message" => append_custom_message(&v, &mut out),
            "compaction" => {
                let summary = v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("compaction");
                out.push(TranscriptLine {
                    role: TranscriptRole::Meta,
                    text: format!("── compaction · {summary}"),
                });
            }
            "branch_summary" => {
                let summary = v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("branch");
                out.push(TranscriptLine {
                    role: TranscriptRole::Meta,
                    text: format!("── branch · {summary}"),
                });
            }
            _ => {}
        }
    }

    if out.is_empty() {
        out.push(TranscriptLine {
            role: TranscriptRole::Meta,
            text: "(no messages yet)".into(),
        });
    }
    out
}

fn append_message(
    v: &Value,
    out: &mut Vec<TranscriptLine>,
    pending_tools: &mut HashMap<String, usize>,
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
            if !out.is_empty() {
                out.push(TranscriptLine {
                    role: TranscriptRole::Meta,
                    text: String::new(),
                });
            }
            for line in text.trim_end().lines() {
                out.push(TranscriptLine {
                    role: TranscriptRole::User,
                    text: line.to_string(),
                });
            }
        }
        "assistant" => append_assistant(message, out, pending_tools),
        "toolResult" => {
            let id = message
                .get("toolCallId")
                .or_else(|| message.get("id"))
                .and_then(|x| x.as_str());
            let ok = message
                .get("isError")
                .and_then(|x| x.as_bool())
                .map(|e| !e)
                .unwrap_or(true);
            let body = content_to_text(message.get("content"));
            let n = body.lines().filter(|l| !l.is_empty()).count().max(1);
            let status = if ok {
                format!("ok · {n} lines")
            } else {
                format!("error · {n} lines")
            };
            if let Some(id) = id {
                if let Some(&idx) = pending_tools.get(id) {
                    if let Some(line) = out.get_mut(idx) {
                        line.text = format!("{} · {status}", line.text);
                    }
                    return;
                }
            }
            out.push(TranscriptLine {
                role: TranscriptRole::Tool,
                text: format!("↳ {status}"),
            });
        }
        _ => {}
    }
}

fn append_assistant(
    message: &Value,
    out: &mut Vec<TranscriptLine>,
    pending_tools: &mut HashMap<String, usize>,
) {
    match message.get("content") {
        Some(Value::String(s)) => {
            let t = s.trim_end();
            if !t.is_empty() {
                for line in t.lines() {
                    out.push(TranscriptLine {
                        role: TranscriptRole::Assistant,
                        text: line.to_string(),
                    });
                }
            }
        }
        Some(Value::Array(parts)) => {
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
                            for line in t.lines() {
                                out.push(TranscriptLine {
                                    role: TranscriptRole::Assistant,
                                    text: line.to_string(),
                                });
                            }
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
                            out.push(TranscriptLine {
                                role: TranscriptRole::Thinking,
                                text: format!("💭 {}", one_line(t, 160)),
                            });
                        }
                    }
                    "toolCall" => {
                        let name = part
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool");
                        let id = part
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args = part.get("arguments").or_else(|| part.get("args"));
                        let arg = format_primary_arg(args);
                        let label = if arg.is_empty() {
                            format!("⚙ {name}")
                        } else {
                            format!("⚙ {name}({arg})")
                        };
                        let idx = out.len();
                        out.push(TranscriptLine {
                            role: TranscriptRole::Tool,
                            text: label,
                        });
                        if !id.is_empty() {
                            pending_tools.insert(id, idx);
                        }
                    }
                    "image" => {
                        out.push(TranscriptLine {
                            role: TranscriptRole::Assistant,
                            text: "[image]".into(),
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn append_custom_message(v: &Value, out: &mut Vec<TranscriptLine>) {
    if v.get("display").and_then(|d| d.as_bool()) == Some(false) {
        return;
    }
    let ctype = v
        .get("customType")
        .and_then(|t| t.as_str())
        .unwrap_or("custom");
    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let one = one_line(content, 120);
    out.push(TranscriptLine {
        role: TranscriptRole::Meta,
        text: if one.is_empty() {
            format!("[{ctype}]")
        } else {
            format!("[{ctype}] {one}")
        },
    });
}

fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(t);
                        }
                    }
                    Some("image") => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str("[image]");
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn format_primary_arg(args: Option<&Value>) -> String {
    let Some(Value::Object(map)) = args else {
        return String::new();
    };
    const KEYS: &[&str] = &[
        "path", "file_path", "filePath", "command", "cmd", "pattern", "url", "query",
        "prompt", "name", "id", "message",
    ];
    for k in KEYS {
        if let Some(Value::String(s)) = map.get(*k) {
            if !s.is_empty() {
                return one_line(s, TOOL_ARG_MAX);
            }
        }
    }
    String::new()
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!(
            "{}…",
            flat.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::omp::TITLE_SLOT_BYTES;
    use std::io::Write;

    fn write_empty_title_slot(f: &mut File) {
        let mut obj = serde_json::json!({
            "type": "title",
            "v": 1,
            "title": "",
            "updatedAt": "t",
            "pad": "",
        });
        for pad_len in 0..TITLE_SLOT_BYTES {
            obj["pad"] = Value::String(" ".repeat(pad_len));
            let mut out = serde_json::to_string(&obj).unwrap().into_bytes();
            out.push(b'\n');
            if out.len() == TITLE_SLOT_BYTES {
                f.write_all(&out).unwrap();
                return;
            }
        }
        panic!("title slot");
    }

    #[test]
    fn parses_user_and_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = File::create(&path).unwrap();
        write_empty_title_slot(&mut f);
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"user","content":[{{"type":"text","text":"hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"text","text":"hi there"}}]}}}}"#
        )
        .unwrap();

        let lines = load_transcript(&path);
        assert!(lines
            .iter()
            .any(|l| l.role == TranscriptRole::User && l.text.contains("hello")));
        assert!(lines
            .iter()
            .any(|l| l.role == TranscriptRole::Assistant && l.text.contains("hi there")));
    }
}
