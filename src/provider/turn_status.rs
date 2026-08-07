//! Derive omp main-turn status and track background jobs from session JSONL.
//!
//! Main-turn status mirrors oh-my-pi `session-listing.ts`
//! `deriveSessionStatus` / `statusFromTailMessage`. Background task events are
//! reduced incrementally because a stopped main turn can still be a scheduling
//! pause while an async result is expected.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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

#[derive(Debug, Default)]
struct ActivityFileState {
    offset: u64,
    outstanding_jobs: HashSet<String>,
}

#[derive(Debug)]
struct TrackedSession {
    path: PathBuf,
    file: ActivityFileState,
}

#[derive(Debug, Default)]
pub struct SessionActivityTracker {
    sessions: HashMap<String, TrackedSession>,
}

#[derive(Debug, Deserialize)]
struct ActivityEntry<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    #[serde(rename = "customType")]
    custom_type: Option<&'a str>,
    #[serde(borrow)]
    message: Option<ActivityMessage<'a>>,
    #[serde(borrow)]
    details: Option<ActivityDetails<'a>>,
}

#[derive(Debug, Deserialize)]
struct ActivityMessage<'a> {
    role: Option<&'a str>,
    #[serde(rename = "toolName")]
    tool_name: Option<&'a str>,
    #[serde(borrow)]
    details: Option<ActivityDetails<'a>>,
}

#[derive(Debug, Deserialize)]
struct ActivityDetails<'a> {
    #[serde(borrow)]
    progress: Option<Vec<ActivityJob<'a>>>,
    #[serde(borrow)]
    jobs: Option<Vec<ActivityJob<'a>>>,
}

#[derive(Debug, Deserialize)]
struct ActivityJob<'a> {
    id: Option<&'a str>,
    #[serde(rename = "jobId")]
    job_id: Option<&'a str>,
    status: Option<&'a str>,
}

impl SessionActivityTracker {
    pub fn busy(
        &mut self,
        session_id: &str,
        live: bool,
        pty_active: bool,
        path: Option<&Path>,
    ) -> bool {
        if !live || !pty_active {
            self.forget(session_id);
            return false;
        }
        let Some(path) = path else {
            self.forget(session_id);
            return false;
        };
        let turn_busy = match derive_disk_turn_status(path) {
            DiskTurnStatus::Pending => true,
            DiskTurnStatus::Interrupted => tools_still_in_flight(path),
            _ => false,
        };
        let async_busy = self.refresh(session_id, path);
        turn_busy || async_busy
    }

    pub fn forget(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    fn refresh(&mut self, session_id: &str, path: &Path) -> bool {
        let needs_reset = self
            .sessions
            .get(session_id)
            .is_none_or(|tracked| tracked.path != path);
        if needs_reset {
            self.sessions.insert(
                session_id.to_owned(),
                TrackedSession {
                    path: path.to_path_buf(),
                    file: ActivityFileState::default(),
                },
            );
        }
        let tracked = self
            .sessions
            .get_mut(session_id)
            .expect("tracked session inserted above");
        refresh_activity_file(path, &mut tracked.file);
        !tracked.file.outstanding_jobs.is_empty()
    }
}

fn refresh_activity_file(path: &Path, state: &mut ActivityFileState) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return;
    };
    if len < state.offset {
        *state = ActivityFileState::default();
    }
    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        return;
    }

    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let line_start = state.offset;
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            return;
        };
        if read == 0 {
            return;
        }
        if line.last() != Some(&b'\n') {
            return;
        }
        state.offset = line_start + read as u64;
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.first() != Some(&b'{') {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<ActivityEntry<'_>>(&line) else {
            continue;
        };
        apply_activity_entry(&entry, &mut state.outstanding_jobs);
    }
}

fn apply_activity_entry(entry: &ActivityEntry<'_>, jobs: &mut HashSet<String>) {
    if entry.kind == Some("custom_message") && entry.custom_type == Some("async-result") {
        if let Some(results) = entry
            .details
            .as_ref()
            .and_then(|details| details.jobs.as_deref())
        {
            for result in results {
                if let Some(id) = result.job_id {
                    jobs.remove(id);
                }
            }
        }
        return;
    }
    if entry.kind != Some("message") {
        return;
    }
    let Some(message) = entry.message.as_ref() else {
        return;
    };
    if message.role != Some("toolResult") {
        return;
    }
    let statuses = match message.tool_name {
        Some("task") => message
            .details
            .as_ref()
            .and_then(|details| details.progress.as_deref()),
        Some("hub") => message
            .details
            .as_ref()
            .and_then(|details| details.jobs.as_deref()),
        _ => None,
    };
    let Some(statuses) = statuses else {
        return;
    };
    for job in statuses {
        apply_job_status(job.id, job.status, jobs);
    }
}

fn apply_job_status(id: Option<&str>, status: Option<&str>, jobs: &mut HashSet<String>) {
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return;
    };
    match status {
        Some("pending" | "running") => {
            if !jobs.contains(id) {
                jobs.insert(id.to_owned());
            }
        }
        Some("completed" | "failed" | "aborted" | "cancelled") => {
            jobs.remove(id);
        }
        _ => {}
    }
}

/// Whether the latest tool loop still has unanswered `toolCall`s.
fn tools_still_in_flight(path: &Path) -> bool {
    let Ok(lines) = read_tail_lines(path) else {
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
                    return false;
                }
                return calls.iter().any(|id| !seen_results.contains(id));
            }
            Some("user") => return false,
            _ => continue,
        }
    }
    true
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
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

pub fn derive_disk_turn_status(path: &Path) -> DiskTurnStatus {
    let Ok(lines) = read_tail_lines(path) else {
        return DiskTurnStatus::Unknown;
    };

    for line in lines.iter().rev() {
        if line.is_empty() || !line.as_bytes().starts_with(b"{") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<TailEntry>(line) else {
            continue;
        };
        if entry.kind.as_deref() == Some("custom_message")
            && entry.custom_type.as_deref() == Some("async-result")
        {
            return DiskTurnStatus::Pending;
        }
        if entry.kind.as_deref() == Some("message") {
            if let Some(message) = entry.message {
                return status_from_tail_message(&message);
            }
        }
    }
    DiskTurnStatus::Unknown
}

#[derive(Debug, Deserialize)]
struct TailEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "customType")]
    custom_type: Option<String>,
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

    fn custom_async_result(job_id: &str) -> String {
        serde_json::json!({
            "type": "custom_message",
            "customType": "async-result",
            "content": "done",
            "details": {"jobs": [{"jobId": job_id, "type": "task"}]}
        })
        .to_string()
    }

    fn assistant_stop() -> String {
        msg(
            "assistant",
            r#","stopReason":"stop","content":[{"type":"text","text":"waiting"}]"#,
        )
    }

    fn task_progress(jobs: &[(&str, &str)]) -> String {
        let progress: Vec<Value> = jobs
            .iter()
            .map(|(id, status)| serde_json::json!({"id": id, "status": status}))
            .collect();
        serde_json::json!({
            "type": "message",
            "message": {
                "role": "toolResult",
                "toolName": "task",
                "details": {"progress": progress}
            }
        })
        .to_string()
    }

    fn hub_jobs(jobs: &[(&str, &str)]) -> String {
        let jobs: Vec<Value> = jobs
            .iter()
            .map(|(id, status)| serde_json::json!({"id": id, "status": status}))
            .collect();
        serde_json::json!({
            "type": "message",
            "message": {
                "role": "toolResult",
                "toolName": "hub",
                "details": {"jobs": jobs}
            }
        })
        .to_string()
    }

    fn append_lines(path: &Path, lines: &[&str]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn yielded_parent_stays_busy_while_task_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let task = task_progress(&[("reviewer", "pending")]);
        let stopped = assistant_stop();
        write_session(&path, &[task.as_str(), stopped.as_str()]);

        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn async_result_clears_the_matching_task_after_final_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let task = task_progress(&[("reviewer", "running")]);
        let stopped = assistant_stop();
        write_session(&path, &[task.as_str(), stopped.as_str()]);
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));

        let result = custom_async_result("reviewer");
        append_lines(&path, &[result.as_str(), stopped.as_str()]);
        assert!(!tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn parallel_tasks_stay_busy_until_every_task_settles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let task = task_progress(&[("one", "pending"), ("two", "running")]);
        let stopped = assistant_stop();
        write_session(&path, &[task.as_str(), stopped.as_str()]);
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));

        let one = custom_async_result("one");
        append_lines(&path, &[one.as_str(), stopped.as_str()]);
        assert!(tracker.busy("session", true, true, Some(&path)));

        let two = custom_async_result("two");
        append_lines(&path, &[two.as_str(), stopped.as_str()]);
        assert!(!tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn hub_terminal_states_clear_jobs() {
        for terminal in ["completed", "failed", "aborted", "cancelled"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t.jsonl");
            let task = task_progress(&[("worker", "running")]);
            let stopped = assistant_stop();
            write_session(&path, &[task.as_str(), stopped.as_str()]);
            let mut tracker = SessionActivityTracker::default();
            assert!(tracker.busy("session", true, true, Some(&path)));

            let hub = hub_jobs(&[("worker", terminal)]);
            append_lines(&path, &[hub.as_str(), stopped.as_str()]);
            assert!(
                !tracker.busy("session", true, true, Some(&path)),
                "terminal state {terminal} must clear the job"
            );
        }
    }

    #[test]
    fn synchronous_completed_task_never_becomes_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let task = task_progress(&[("inline", "completed")]);
        let stopped = assistant_stop();
        write_session(&path, &[task.as_str(), stopped.as_str()]);

        let mut tracker = SessionActivityTracker::default();
        assert!(!tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn tracker_waits_for_a_complete_appended_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let stopped = assistant_stop();
        write_session(&path, &[stopped.as_str()]);
        let mut tracker = SessionActivityTracker::default();
        assert!(!tracker.busy("session", true, true, Some(&path)));

        let task = task_progress(&[("reviewer", "pending")]);
        let split = task.len() / 2;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(task[..split].as_bytes()).unwrap();
        drop(file);
        assert!(!tracker.busy("session", true, true, Some(&path)));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(task[split..].as_bytes()).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{stopped}").unwrap();
        drop(file);
        assert!(tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn tracker_rebuilds_after_file_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let task = task_progress(&[("reviewer", "pending")]);
        let stopped = assistant_stop();
        write_session(&path, &[task.as_str(), stopped.as_str()]);
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));

        write_session(&path, &[stopped.as_str()]);
        assert!(!tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn malformed_and_unknown_events_preserve_confirmed_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let task = task_progress(&[("reviewer", "pending")]);
        let stopped = assistant_stop();
        write_session(&path, &[task.as_str(), stopped.as_str()]);
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));

        let unknown = task_progress(&[("reviewer", "paused")]);
        append_lines(&path, &["{broken", unknown.as_str(), stopped.as_str()]);
        assert!(tracker.busy("session", true, true, Some(&path)));
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
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));
        assert!(!tracker.busy("session", false, false, Some(&path)));
        assert!(!tracker.busy("session", true, false, Some(&path))); // exited
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
        let mut tracker = SessionActivityTracker::default();
        assert!(!tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn async_result_is_pending_until_assistant_replies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let asst = msg(
            "assistant",
            r#","stopReason":"stop","content":[{"type":"text","text":"waiting"}]"#,
        );
        let delivered = custom_async_result("reviewer");
        write_session(&path, &[asst.as_str(), delivered.as_str()]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Pending);
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
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn trailing_tool_results_all_answered_not_busy() {
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
        let mut tracker = SessionActivityTracker::default();
        assert!(!tracker.busy("s", true, true, Some(&path)));
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
        let mut tracker = SessionActivityTracker::default();
        assert!(tracker.busy("s", true, true, Some(&path)));
    }


    #[test]
    fn aborted_and_error_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = msg("user", r#","content":"go""#);
        let asst = msg("assistant", r#","stopReason":"aborted","content":[]"#);
        write_session(&path, &[user.as_str(), asst.as_str()]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Aborted);
        let mut tracker = SessionActivityTracker::default();
        assert!(!tracker.busy("session", true, true, Some(&path)));
    }

    #[test]
    fn unknown_never_spins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_session(&path, &[]);
        assert_eq!(derive_disk_turn_status(&path), DiskTurnStatus::Unknown);
        let mut tracker = SessionActivityTracker::default();
        assert!(!tracker.busy("session", true, true, Some(&path)));
        assert!(!tracker.busy("session", true, true, None));
    }
}
