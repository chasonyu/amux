//! Derive whether an omp agent turn is still in progress from the JSONL tail.
//!
//! Mirrors oh-my-pi `session-listing.ts` `deriveSessionStatus` / `statusFromTailMessage`.
//! amux only surfaces a spinner for **live** sessions when the tail says the
//! agent still owes work (`pending` / `interrupted`). `unknown` never lights
//! the spinner (prefer false idle over false busy).

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
/// - Only `pending` (trailing user) or `interrupted` (mid tool loop)
/// - `unknown` / `complete` / `aborted` / `error` → no spinner
pub fn agent_turn_busy(live: bool, pty_active: bool, path: Option<&Path>) -> bool {
    if !live || !pty_active {
        return false;
    }
    let Some(path) = path else {
        return false;
    };
    matches!(
        derive_disk_turn_status(path),
        DiskTurnStatus::Pending | DiskTurnStatus::Interrupted
    )
}

pub fn derive_disk_turn_status(path: &Path) -> DiskTurnStatus {
    let Ok(mut f) = File::open(path) else {
        return DiskTurnStatus::Unknown;
    };
    let Ok(meta) = f.metadata() else {
        return DiskTurnStatus::Unknown;
    };
    let len = meta.len();
    if len == 0 {
        return DiskTurnStatus::Unknown;
    }
    let start = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return DiskTurnStatus::Unknown;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return DiskTurnStatus::Unknown;
    }
    // When we started mid-file, the first line may be a partial fragment —
    // skip it (same as omp walking lines and ignoring non-`{` starts).
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = if start > 0 {
        let mut it = text.split('\n');
        let _ = it.next(); // drop leading partial
        it.collect()
    } else {
        text.split('\n').collect()
    };

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
        return status_from_tail_message(&message);
    }
    DiskTurnStatus::Unknown
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
    let Some(Value::Array(parts)) = content else {
        return false;
    };
    parts.iter().any(|p| {
        p.get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "toolCall")
    })
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
