//! Derive whether an omp agent turn is still in progress from the JSONL tail.
//!
//! Listing status mirrors oh-my-pi `session-listing.ts`
//! `deriveSessionStatus` / `statusFromTailMessage`.
//!
//! Sidebar spinner (`agent_turn_busy`) is stricter than listing status:
//! trailing `toolResult` only counts as busy while the previous assistant
//! turn still has unanswered `toolCall`s. Once every tool has returned, we
//! stop the wave even if omp never wrote a final assistant message (stuck /
//! abandoned mid-turn) — prefer brief false-idle over infinite false-busy.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// omp `SESSION_LIST_SUFFIX_BYTES` — if the final message exceeds this window,
/// status is [`DiskTurnStatus::Unknown`] rather than a wrong classification.
const TAIL_BYTES: u64 = 32_768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskTurnStatus {
    Complete,
    Interrupted,
    Aborted,
    Error,
    Pending,
    Unknown,
}

/// True when the sidebar should show an in-progress spinner.
///
/// Strict gates (avoid “idle shown as busy” / “busy shown as idle” where we can):
/// - `live && pty_active` (Starting/Running); Exited/Disk never spin
/// - JSONL path required (no guess for pre-file synthetic sessions)
/// - `pending` (trailing user) → busy
/// - `interrupted` with unanswered toolCalls (assistant tail or partial toolResults) → busy
/// - trailing `toolResult` with all toolCalls answered → **not** busy
/// - `unknown` / `complete` / `aborted` / `error` → no spinner
pub fn agent_turn_busy(live: bool, pty_active: bool, path: Option<&Path>) -> bool {
    if !live || !pty_active {
        return false;
    }
    let Some(path) = path else {
        return false;
    };
    match derive_disk_turn_status(path) {
        DiskTurnStatus::Pending => true,
        DiskTurnStatus::Interrupted => tools_still_in_flight(path),
        _ => false,
    }
}

pub fn derive_disk_turn_status(path: &Path) -> DiskTurnStatus {
    let Some(message) = last_tail_message(path) else {
        return DiskTurnStatus::Unknown;
    };
    status_from_tail_message(&message)
}

/// Whether the latest tool loop still has unanswered `toolCall`s.
///
/// - Trailing assistant with `toolCall` → true (tools not finished writing results)
/// - Trailing `toolResult`s → true only if the preceding assistant still has
///   a `toolCall` id without a matching result
/// - Otherwise → false (including “all tools returned, waiting on model”)
fn tools_still_in_flight(path: &Path) -> bool {
    let Ok(lines) = read_tail_lines(path) else {
        // Fail closed on I/O: keep prior Interrupted semantics (busy).
        return true;
    };
    let mut seen_results: HashSet<String> = HashSet::new();
    for line in lines.iter().rev() {
        if line.is_empty() || !line.as_bytes().starts_with(b"{") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<TailEntry>(line) else {
            continue;
        };
        if entry.kind.as_deref() != Some("message") {
            continue;
        }
        let Some(message) = entry.message else {
            continue;
        };
        match message.role.as_deref() {
            Some("toolResult") => {
                if let Some(id) = message.tool_call_id.filter(|s| !s.is_empty()) {
                    seen_results.insert(id);
                }
            }
            Some("assistant") => {
                let calls = tool_call_ids(message.content.as_ref());
                if calls.is_empty() {
                    // Text-only / aborted assistant — not a live tool loop.
                    return false;
                }
                return calls.iter().any(|id| !seen_results.contains(id));
            }
            Some("user") => return false,
            _ => continue,
        }
    }
    // Interrupted (e.g. length truncate) without a resolvable tool loop —
    // keep spinning rather than falsely clearing a mid-flight turn.
    true
}

fn last_tail_message(path: &Path) -> Option<TailMessage> {
    let lines = read_tail_lines(path).ok()?;
    for line in lines.iter().rev() {
        if line.is_empty() || !line.as_bytes().starts_with(b"{") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<TailEntry>(line) else {
            continue;
        };
        if entry.kind.as_deref() != Some("message") {
            continue;
        }
        if let Some(message) = entry.message {
            return Some(message);
        }
    }
    None
}

fn read_tail_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let mut f = File::open(path)?;
    let meta = f.metadata()?;
    let len = meta.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    // When we started mid-file, the first line may be a partial fragment.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

#[derive(Debug, Deserialize)]
struct TailEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<TailMessage>,
}

#[derive(Debug, Deserialize)]
struct TailMessage {
    role: Option<String>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    content: Option<Value>,
    #[serde(rename = "toolCallId")]
    tool_call_id: Option<String>,
}

fn status_from_tail_message(message: &TailMessage) -> DiskTurnStatus {
    match message.role.as_deref() {
        Some("assistant") => match message.stop_reason.as_deref() {
            Some("error") => DiskTurnStatus::Error,
            Some("aborted") => DiskTurnStatus::Aborted,
            Some("length") => DiskTurnStatus::Interrupted,
            _ => {
                if content_has_tool_call(message.content.as_ref()) {
                    DiskTurnStatus::Interrupted
                } else {
                    DiskTurnStatus::Complete
                }
            }
        },
        Some("toolResult") => DiskTurnStatus::Interrupted,
        Some("user") => DiskTurnStatus::Pending,
        _ => DiskTurnStatus::Unknown,
    }
}

fn content_has_tool_call(content: Option<&Value>) -> bool {
    !tool_call_ids(content).is_empty()
}

fn tool_call_ids(content: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(parts)) = content else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|p| {
            if p.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
                return None;
            }
            p.get("id")
                .and_then(|id| id.as_str())
                .filter(|id| !id.is_empty())
                .map(|id| id.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::omp::TITLE_SLOT_BYTES;
    use std::io::Write;

    // Re-export helper: tests need a title slot — use local pad builder.
    fn write_session(path: &Path, body_lines: &[&str]) {
        // Minimal valid-ish file: empty title slot + session header + messages.
        let mut slot = None;
        for pad_len in 0..TITLE_SLOT_BYTES {
            let obj = serde_json::json!({
                "type": "title",
                "v": 1,
                "title": "",
                "updatedAt": "2026-08-01T00:00:00.000Z",
                "pad": " ".repeat(pad_len),
            });
            let mut out = serde_json::to_string(&obj).unwrap().into_bytes();
            out.push(b'\n');
            if out.len() == TITLE_SLOT_BYTES {
                slot = Some(out);
                break;
            }
        }
        let mut f = File::create(path).unwrap();
        f.write_all(&slot.unwrap()).unwrap();
        writeln!(f, r#"{{"type":"session","version":3,"id":"abc","cwd":"/p","timestamp":"2026-08-01T00:00:00.000Z"}}"#).unwrap();
        for line in body_lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn msg(role: &str, extra: &str) -> String {
        format!(
            r#"{{"type":"message","id":"e1","parentId":null,"timestamp":"2026-08-01T00:00:00.000Z","message":{{"role":"{role}"{extra}}}}}"#
        )
    }

    #[test]
    fn pending_user_is_busy_when_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_session(
            &path,
            &[msg("user", r#","content":"still waiting""#).as_str()],
        );
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Pending);
        assert!(agent_turn_busy(true, true, Some(&path)));
        assert!(!agent_turn_busy(false, false, Some(&path)));
        assert!(!agent_turn_busy(true, false, Some(&path))); // exited
    }

    #[test]
    fn complete_assistant_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = msg("user", r#","content":"hi""#);
        let asst = msg(
            "assistant",
            r#","stopReason":"stop","content":[{"type":"text","text":"done"}]"#,
        );
        write_session(&path, &[user.as_str(), asst.as_str()]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Complete);
        assert!(!agent_turn_busy(true, true, Some(&path)));
    }

    #[test]
    fn tool_call_assistant_is_interrupted_busy_when_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = msg("user", r#","content":"go""#);
        let asst = msg(
            "assistant",
            r#","stopReason":"toolUse","content":[{"type":"toolCall","id":"t1","name":"read","arguments":{}}]"#,
        );
        write_session(&path, &[user.as_str(), asst.as_str()]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Interrupted);
        assert!(agent_turn_busy(true, true, Some(&path)));
    }

    #[test]
    fn trailing_tool_results_all_answered_not_busy() {
        // Regression: session 019fdb0c… ended with toolResults and never got a
        // final assistant message — listing stays Interrupted, spinner must stop.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = msg("user", r#","content":"go""#);
        let asst = msg(
            "assistant",
            r#","stopReason":"toolUse","content":[{"type":"toolCall","id":"t1","name":"read","arguments":{}},{"type":"toolCall","id":"t2","name":"bash","arguments":{}}]"#,
        );
        let r1 = msg(
            "toolResult",
            r#","toolCallId":"t1","toolName":"read","content":[{"type":"text","text":"ok"}]"#,
        );
        let r2 = msg(
            "toolResult",
            r#","toolCallId":"t2","toolName":"bash","content":[{"type":"text","text":"ok"}]"#,
        );
        write_session(
            &path,
            &[user.as_str(), asst.as_str(), r1.as_str(), r2.as_str()],
        );
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Interrupted);
        assert!(!agent_turn_busy(true, true, Some(&path)));
    }

    #[test]
    fn trailing_tool_results_partial_still_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = msg("user", r#","content":"go""#);
        let asst = msg(
            "assistant",
            r#","stopReason":"toolUse","content":[{"type":"toolCall","id":"t1","name":"read","arguments":{}},{"type":"toolCall","id":"t2","name":"bash","arguments":{}}]"#,
        );
        let r1 = msg(
            "toolResult",
            r#","toolCallId":"t1","toolName":"read","content":[{"type":"text","text":"ok"}]"#,
        );
        write_session(&path, &[user.as_str(), asst.as_str(), r1.as_str()]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Interrupted);
        assert!(agent_turn_busy(true, true, Some(&path)));
    }

    #[test]
    fn aborted_and_error_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = msg("user", r#","content":"go""#);
        let asst = msg("assistant", r#","stopReason":"aborted","content":[]"#);
        write_session(&path, &[user.as_str(), asst.as_str()]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Aborted);
        assert!(!agent_turn_busy(true, true, Some(&path)));
    }

    #[test]
    fn unknown_never_spins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_session(&path, &[]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Unknown);
        assert!(!agent_turn_busy(true, true, Some(&path)));
        assert!(!agent_turn_busy(true, true, None));
    }

}
