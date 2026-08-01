//! Shared JSONL content helpers for transcript parsers.

use serde_json::Value;

use super::TRUNCATE_ARG;

/// Flatten message `content` (string or text/image parts) into display text.
pub(super) fn content_to_text(content: Option<&Value>) -> String {
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

/// Pick a short primary argument for tool cards (path/command/…).
pub(super) fn format_primary_arg(args: Option<&Value>) -> String {
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
                return one_line(s, TRUNCATE_ARG);
            }
        }
    }
    String::new()
}

/// Collapse whitespace to one line and truncate to `max` chars.
pub(super) fn one_line(text: &str, max: usize) -> String {
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

/// First path-like argument for read tools.
pub(super) fn read_path_arg(args: Option<&Value>) -> Option<String> {
    let Value::Object(map) = args? else {
        return None;
    };
    for k in ["path", "file_path", "filePath"] {
        if let Some(Value::String(s)) = map.get(k) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}
