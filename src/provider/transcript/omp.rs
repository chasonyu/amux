//! omp JSONL → [`TranscriptBlock`].

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::provider::omp::skip_title_prefix;

use super::util::{content_to_text, format_primary_arg, one_line, read_path_arg};
use super::{
    clamp_hunk_text, render_hunks, DiffKind, DiffLine, FileHunk, FileOp, ModifiedFile, ToolKind,
    ToolStatus, TranscriptBlock, COLLAPSED_LINES, FILES_MAX, HUNKS_PER_FILE_MAX,
    HUNK_BYTES_PER_FILE_MAX, OUTPUT_COLLAPSED, TRUNCATE_LINE, TRUNCATE_TITLE,
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
    let mut bytes = Vec::new();
    let _ = limited.read_to_end(&mut bytes);
    // Lossy on purpose: the byte cap can land mid UTF-8 char, and
    // `read_to_string` would then fail and leave the buffer empty — an entire
    // transcript lost to one truncated character.
    let buf = String::from_utf8_lossy(&bytes);

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

/// Files touched by successful `edit` / `write` calls in a session JSONL,
/// aggregated by path.
///
/// Scanning is incremental: JSONL is append-only, so each [`poll`] parses only
/// the bytes added since the previous one. That matters for live sessions,
/// where the panel repaints on every PTY burst and a full re-parse of a
/// multi-megabyte transcript per frame would be unaffordable.
///
/// A change is aggregated only once its `toolResult` says it landed — omp
/// records `isError` there, and failed calls (bad args, no matching text)
/// never touched the file.
///
/// [`poll`]: ModifiedFilesScan::poll
pub struct ModifiedFilesScan {
    /// Session cwd, for making recorded paths relative.
    cwd: PathBuf,
    /// Bytes already parsed, always on a line boundary.
    offset: u64,
    agg: HashMap<String, Agg>,
    /// Calls waiting for their `toolResult`.
    pending: VecDeque<PendingCall>,
    files: Vec<ModifiedFile>,
    version: u64,
    /// Monotonic commit counter driving the [`FILES_MAX`] eviction order.
    seq: u64,
}

/// Per-path aggregate. `hunks` is bounded by [`HUNKS_PER_FILE_MAX`] /
/// [`HUNK_BYTES_PER_FILE_MAX`]; whatever is dropped is counted in `omitted`.
struct Agg {
    count: usize,
    last_op: FileOp,
    last_time: Option<String>,
    hunks: Vec<FileHunk>,
    bytes: usize,
    omitted: usize,
    seq: u64,
}

struct PendingCall {
    id: String,
    op: FileOp,
    time: Option<String>,
    /// One call may address several files (patch DSL with multiple headers).
    targets: Vec<(String, Vec<FileHunk>)>,
    /// False for patch DSL headers, whose path is whatever the agent typed;
    /// true for `write` / `edits[]`, which record the real path.
    authoritative: bool,
}

/// Calls kept while waiting for results. Deep enough for parallel tool calls;
/// a call whose result never arrives (aborted turn) ages out.
const PENDING_MAX: usize = 64;
/// Lines longer than this are skipped rather than parsed — a legitimate omp
/// record stays well under it, and this bounds the JSON parse cost.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Diff lines kept per patch DSL block.
const PATCH_LINES_MAX: usize = 2_000;

impl ModifiedFilesScan {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            offset: 0,
            agg: HashMap::new(),
            pending: VecDeque::new(),
            files: Vec::new(),
            version: 0,
            seq: 0,
        }
    }

    /// Aggregated files, most recently modified first.
    pub fn files(&self) -> &[ModifiedFile] {
        &self.files
    }

    /// Bumped whenever [`files`](Self::files) or the retained changes change;
    /// callers use it to invalidate rendered diffs.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Diff lines for the file at `index` in [`files`](Self::files).
    pub fn file_diff(&self, index: usize) -> Vec<DiffLine> {
        let Some(file) = self.files.get(index) else {
            return Vec::new();
        };
        let Some(agg) = self.agg.get(&file.path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if agg.omitted > 0 {
            out.push(DiffLine {
                kind: DiffKind::Context,
                text: format!(
                    "… {} earlier change(s) dropped to bound memory",
                    agg.omitted
                ),
            });
        }
        out.extend(render_hunks(&agg.hunks));
        out
    }

    /// Parse everything appended since the last poll; returns true when the
    /// aggregate changed. A shorter file (rewritten / rotated) restarts the
    /// scan from the top.
    pub fn poll(&mut self, path: &Path) -> bool {
        let Ok(mut f) = File::open(path) else {
            return false;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.reset();
        }
        if len == self.offset {
            return false;
        }
        if self.offset == 0 {
            if skip_title_prefix(&mut f).is_err() {
                let _ = f.seek(SeekFrom::Start(0));
            }
            self.offset = f.stream_position().unwrap_or(0);
        } else if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }

        // Line-at-a-time on raw bytes: a size cap on a whole-file read would
        // both hide later edits and — when the cut lands mid UTF-8 char — make
        // `read_to_string` fail and yield nothing at all.
        let mut reader = BufReader::new(f);
        let mut line = Vec::new();
        let mut changed = false;
        loop {
            line.clear();
            let Ok(n) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            if n == 0 || !line.ends_with(b"\n") {
                // Trailing partial line: re-read it whole on the next poll.
                break;
            }
            self.offset += n as u64;
            if n <= MAX_LINE_BYTES {
                changed |= self.consume_line(&line);
            }
        }
        if changed {
            self.rebuild();
        }
        changed
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.agg.clear();
        self.pending.clear();
        self.files.clear();
        self.seq = 0;
        // Rendered diffs keyed on the old version must not survive the restart.
        self.version += 1;
    }

    fn consume_line(&mut self, raw: &[u8]) -> bool {
        let text = String::from_utf8_lossy(raw);
        // Only tool calls and their results matter, and both spell the tool
        // name — skips read/bash payloads without paying for a JSON parse.
        if !text.contains("\"edit\"") && !text.contains("\"write\"") {
            return false;
        }
        let Ok(v) = serde_json::from_str::<Value>(text.trim_end()) else {
            return false;
        };
        let Some(message) = v.get("message") else {
            return false;
        };
        match message.get("role").and_then(|r| r.as_str()) {
            Some("assistant") => {
                let ts = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                self.queue_calls(message, ts);
                false
            }
            Some("toolResult") => self.apply_result(message),
            _ => false,
        }
    }

    fn queue_calls(&mut self, message: &Value, ts: Option<String>) {
        let Some(parts) = message.get("content").and_then(|c| c.as_array()) else {
            return;
        };
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
                continue;
            }
            let op = match part.get("name").and_then(|n| n.as_str()) {
                Some("write") => FileOp::Write,
                Some("edit") => FileOp::Edit,
                _ => continue,
            };
            let Some(id) = part.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            let args = part.get("arguments").or_else(|| part.get("args"));
            let targets = extract_targets(args, op, &self.cwd);
            if targets.is_empty() {
                continue;
            }
            let authoritative = op == FileOp::Write || !has_patch_input(args);
            if self.pending.len() >= PENDING_MAX {
                self.pending.pop_front();
            }
            self.pending.push_back(PendingCall {
                id: id.to_string(),
                op,
                time: ts.clone(),
                targets,
                authoritative,
            });
        }
    }

    fn apply_result(&mut self, message: &Value) -> bool {
        let Some(id) = message.get("toolCallId").and_then(|i| i.as_str()) else {
            return false;
        };
        let Some(pos) = self.pending.iter().position(|p| p.id == id) else {
            return false;
        };
        let Some(call) = self.pending.remove(pos) else {
            return false;
        };
        if message
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            return false;
        }
        for (path, hunks) in call.targets {
            self.commit(path, call.op, call.time.clone(), hunks, call.authoritative);
        }
        true
    }

    fn commit(
        &mut self,
        path: String,
        op: FileOp,
        time: Option<String>,
        hunks: Vec<FileHunk>,
        authoritative: bool,
    ) {
        let key = self.resolve_key(path, authoritative);
        self.seq += 1;
        let seq = self.seq;
        let entry = self.agg.entry(key).or_insert_with(|| Agg::new(op, seq));
        entry.count += 1;
        entry.last_op = op;
        entry.seq = seq;
        if time.is_some() {
            entry.last_time = time;
        }
        if op == FileOp::Write {
            // A full-content write supersedes everything kept for this file.
            entry.hunks.clear();
            entry.bytes = 0;
            entry.omitted = 0;
        }
        for h in hunks {
            entry.bytes += h.bytes();
            entry.hunks.push(h);
        }
        entry.trim();
        if self.agg.len() > FILES_MAX {
            self.evict_oldest();
        }
    }

    /// Aggregation key for a committed path.
    ///
    /// A patch DSL header carries whatever the agent passed in — often just a
    /// file name or a trailing fragment — while `write` records the full
    /// relative path. Such a short form folds onto the longer path it uniquely
    /// matches so a file stays one row; when several files could match, the
    /// rows stay apart rather than guessing. An `authoritative` path is never
    /// folded upward: `src/a.rs` and `x/src/a.rs` are different files.
    fn resolve_key(&mut self, path: String, authoritative: bool) -> String {
        if !authoritative {
            if let Some(full) = self.unique_match_for(&path) {
                return full;
            }
        }
        // The reverse case: short forms aggregated before the full path showed
        // up. Safe only while no *other* known path ends in the same fragment.
        let mergeable: Vec<String> = self
            .agg
            .keys()
            .filter(|k| path.ends_with(&format!("/{k}")) && self.suffix_match_count(k) == 0)
            .cloned()
            .collect();
        for s in mergeable {
            if let Some(agg) = self.agg.remove(&s) {
                self.merge_into(&path, agg);
            }
        }
        path
    }

    /// The single aggregated path ending in `/short`, if there is exactly one.
    fn unique_match_for(&self, short: &str) -> Option<String> {
        let suffix = format!("/{short}");
        let mut hit = None;
        for k in self.agg.keys() {
            if k.ends_with(&suffix) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(k.clone());
            }
        }
        hit
    }

    /// How many aggregated paths end in `/short`.
    fn suffix_match_count(&self, short: &str) -> usize {
        let suffix = format!("/{short}");
        self.agg.keys().filter(|k| k.ends_with(&suffix)).count()
    }

    /// Absorb a short-form aggregate into the full-path row for the same file.
    fn merge_into(&mut self, path: &str, short: Agg) {
        let entry = self
            .agg
            .entry(path.to_string())
            .or_insert_with(|| Agg::new(short.last_op, short.seq));
        entry.count += short.count;
        if short.last_time > entry.last_time {
            entry.last_time = short.last_time;
            entry.last_op = short.last_op;
        }
        let mut hunks = short.hunks;
        hunks.append(&mut entry.hunks);
        entry.hunks = hunks;
        entry.bytes += short.bytes;
        entry.omitted += short.omitted;
        entry.trim();
    }

    /// Drop the least recently touched path once past [`FILES_MAX`].
    fn evict_oldest(&mut self) {
        let Some(key) = self
            .agg
            .iter()
            .min_by_key(|(_, a)| a.seq)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        self.agg.remove(&key);
    }

    /// Rebuild the display rows. Rows hold no change text, so this stays cheap
    /// even though it runs on every batch of committed changes.
    fn rebuild(&mut self) {
        self.version += 1;
        self.files = self
            .agg
            .iter()
            .map(|(path, a)| ModifiedFile {
                path: path.clone(),
                count: a.count,
                last_op: a.last_op,
                last_time: a.last_time.clone(),
            })
            .collect();
        // Most recently modified first; ties by path for stable order.
        self.files.sort_by(|a, b| {
            b.last_time
                .cmp(&a.last_time)
                .then_with(|| a.path.cmp(&b.path))
        });
    }
}

impl Agg {
    fn new(op: FileOp, seq: u64) -> Self {
        Self {
            count: 0,
            last_op: op,
            last_time: None,
            hunks: Vec::new(),
            bytes: 0,
            omitted: 0,
            seq,
        }
    }

    fn trim(&mut self) {
        while self.hunks.len() > HUNKS_PER_FILE_MAX
            || (self.bytes > HUNK_BYTES_PER_FILE_MAX && self.hunks.len() > 1)
        {
            let dropped = self.hunks.remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.bytes());
            self.omitted += 1;
        }
    }
}

/// True when an `edit` call uses the patch DSL rather than an `edits[]` array.
fn has_patch_input(args: Option<&Value>) -> bool {
    matches!(args, Some(Value::Object(map)) if map.get("input").and_then(|i| i.as_str()).is_some())
}

/// Files and raw changes for one `edit` / `write` call, keyed by normalized
/// path. Returns empty when nothing real is addressed (internal URI, missing
/// path, unparsable payload).
fn extract_targets(args: Option<&Value>, op: FileOp, cwd: &Path) -> Vec<(String, Vec<FileHunk>)> {
    let Some(Value::Object(map)) = args else {
        return Vec::new();
    };
    match op {
        FileOp::Write => {
            let Some(path) = map
                .get("path")
                .and_then(|p| p.as_str())
                .and_then(|p| normalize_path(p, cwd))
            else {
                return Vec::new();
            };
            let hunks = match map.get("content").and_then(|c| c.as_str()) {
                Some(c) if !c.is_empty() => vec![FileHunk::Content(clamp_hunk_text(c))],
                _ => Vec::new(),
            };
            vec![(path, hunks)]
        }
        FileOp::Edit => {
            // Current omp: line-addressed patch DSL in `input`.
            if let Some(input) = map.get("input").and_then(|i| i.as_str()) {
                let mut out: Vec<(String, Vec<FileHunk>)> = Vec::new();
                for (raw, lines) in parse_patch(input) {
                    let Some(path) = normalize_path(&raw, cwd) else {
                        continue;
                    };
                    match out.iter_mut().find(|(p, _)| *p == path) {
                        Some((_, hunks)) => hunks.push(FileHunk::Patch(lines)),
                        None => out.push((path, vec![FileHunk::Patch(lines)])),
                    }
                }
                return out;
            }
            // Older omp: `path` plus an `edits` array of old/new pairs.
            let Some(path) = map
                .get("path")
                .and_then(|p| p.as_str())
                .and_then(|p| normalize_path(p, cwd))
            else {
                return Vec::new();
            };
            let mut hunks = Vec::new();
            if let Some(Value::Array(arr)) = map.get("edits") {
                for h in arr {
                    let old = h.get("old_text").and_then(|t| t.as_str()).unwrap_or("");
                    let new = h.get("new_text").and_then(|t| t.as_str()).unwrap_or("");
                    if old.is_empty() && new.is_empty() {
                        continue;
                    }
                    hunks.push(FileHunk::Replace {
                        old: clamp_hunk_text(old),
                        new: clamp_hunk_text(new),
                    });
                }
            }
            vec![(path, hunks)]
        }
    }
}

/// Split omp's `edit` patch DSL into per-file diff lines:
///
/// ```text
/// [src/main.rs#C051]   file header (content fingerprint optional)
/// SWAP 341.=349:       line-addressed op
/// +  new text          replacement / inserted line
/// CUT 26               deletion by line number
/// ```
///
/// The payload carries no old text, so ops are surfaced as context lines —
/// without them a pure `CUT` would render as an empty diff. Lines before the
/// first header are dropped.
fn parse_patch(input: &str) -> Vec<(String, Vec<DiffLine>)> {
    let mut out: Vec<(String, Vec<DiffLine>)> = Vec::new();
    for line in input.lines() {
        if let Some(path) = patch_header_path(line) {
            out.push((path.to_string(), Vec::new()));
            continue;
        }
        let Some((_, lines)) = out.last_mut() else {
            continue;
        };
        if lines.len() >= PATCH_LINES_MAX {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            lines.push(DiffLine {
                kind: DiffKind::Add,
                text: rest.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix('-') {
            lines.push(DiffLine {
                kind: DiffKind::Del,
                text: rest.to_string(),
            });
        } else if !line.trim().is_empty() {
            lines.push(DiffLine {
                kind: DiffKind::Context,
                text: line.to_string(),
            });
        }
    }
    out
}

/// `[path#A1B2]` / `[path]` → path. The trailing `#…` is omp's content
/// fingerprint (sometimes literal `XXXX`), not part of the path.
fn patch_header_path(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() {
        return None;
    }
    match inner.rsplit_once('#') {
        Some((path, fingerprint))
            if !path.is_empty()
                && !fingerprint.is_empty()
                && fingerprint
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == 'X') =>
        {
            Some(path)
        }
        _ => Some(inner),
    }
}

/// Aggregation key and display path for a recorded path argument.
///
/// Values containing `://` are dropped: omp's `write` tool doubles as an
/// internal MCP channel (`xd://`, `conflict://`, `local://`) and those calls
/// touch no file. Paths under the session cwd are made relative so the same
/// file recorded both ways aggregates into one row.
fn normalize_path(raw: &str, cwd: &Path) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains("://") {
        return None;
    }
    let raw = raw.strip_prefix("./").unwrap_or(raw);
    let p = Path::new(raw);
    let rel = match p.strip_prefix(cwd) {
        Ok(r) if p.is_absolute() => r,
        _ => p,
    };
    let s = rel.to_string_lossy();
    if s.is_empty() {
        return None;
    }
    Some(s.into_owned())
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
    use std::fs;
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

    /// Fixture cwd — absolute paths recorded under it must fold into the
    /// relative row for the same file.
    const FIXTURE_CWD: &str = "/home/dev/proj";

    fn fixture_scan() -> ModifiedFilesScan {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/provider/transcript/fixtures/modified_files.jsonl");
        let mut scan = ModifiedFilesScan::new(Path::new(FIXTURE_CWD));
        assert!(scan.poll(&path), "expected the fixture to yield changes");
        scan
    }

    #[test]
    fn aggregates_only_landed_changes() {
        let scan = fixture_scan();
        let names: Vec<&str> = scan.files().iter().map(|f| f.path.as_str()).collect();
        // Dropped: the xd:// MCP write, both isError calls (missing path /
        // src/ghost.js) and the in-flight write with no result yet.
        assert_eq!(names, vec!["config.toml", "src/main.js", "src/shaders.js"]);

        // shaders.js: write + legacy edits[] + patch DSL recorded absolute.
        let sh = scan
            .files()
            .iter()
            .find(|f| f.path == "src/shaders.js")
            .unwrap();
        assert_eq!(sh.count, 3);
        assert_eq!(sh.last_op, FileOp::Edit);
        assert_eq!(sh.last_time.as_deref(), Some("2026-08-01T13:21:28.842Z"));

        // main.js: write then a patch call that also touched config.toml.
        let main = scan
            .files()
            .iter()
            .find(|f| f.path == "src/main.js")
            .unwrap();
        assert_eq!(main.count, 2);
        assert_eq!(main.last_op, FileOp::Edit);
        assert_eq!(main.last_time.as_deref(), Some("2026-08-01T13:39:03.914Z"));
    }

    #[test]
    fn patch_diff_keeps_ops_as_context_and_new_lines_as_adds() {
        let scan = fixture_scan();
        let idx = scan
            .files()
            .iter()
            .position(|f| f.path == "src/shaders.js")
            .unwrap();
        let diff = scan.file_diff(idx);
        // Line-addressed ops have no old text; they show as context so a pure
        // CUT is still visible.
        assert!(
            diff.iter()
                .any(|l| l.kind == DiffKind::Context && l.text == "CUT 9"),
            "got {diff:?}"
        );
        assert!(diff
            .iter()
            .any(|l| l.kind == DiffKind::Add && l.text == "line3b"));
        // Legacy edits[] pair still diffs into add/del.
        assert!(diff
            .iter()
            .any(|l| l.kind == DiffKind::Add && l.text == "line3"));
    }

    #[test]
    fn write_supersedes_earlier_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut body = String::new();
        body.push_str(&assistant_call(
            "t1",
            "e1",
            r#"{"i":"x","input":"[a.rs#1A]\nCUT 4"}"#,
            "edit",
        ));
        body.push_str(&tool_result("e1", "edit", false));
        body.push_str(&assistant_call(
            "t2",
            "w1",
            r#"{"path":"a.rs","content":"fresh\n"}"#,
            "write",
        ));
        body.push_str(&tool_result("w1", "write", false));
        fs::write(&path, body).unwrap();

        let mut scan = ModifiedFilesScan::new(dir.path());
        assert!(scan.poll(&path));
        assert_eq!(scan.files().len(), 1);
        assert_eq!(scan.files()[0].count, 2);
        assert_eq!(scan.files()[0].last_op, FileOp::Write);
        // The full-content write replaced the file, so the earlier patch is gone
        // instead of being stacked on top of it.
        let diff = scan.file_diff(0);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].kind, DiffKind::Add);
        assert_eq!(diff[0].text, "fresh");
    }

    #[test]
    fn poll_is_incremental_and_survives_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut head = assistant_call("t1", "w1", r#"{"path":"a.rs","content":"one\n"}"#, "write");
        head.push_str(&tool_result("w1", "write", false));
        fs::write(&path, &head).unwrap();

        let mut scan = ModifiedFilesScan::new(dir.path());
        assert!(scan.poll(&path));
        let v1 = scan.version();
        assert_eq!(scan.files().len(), 1);
        // Nothing appended → no work, no version bump.
        assert!(!scan.poll(&path));
        assert_eq!(scan.version(), v1);

        // A half-written record must wait for its newline instead of being
        // dropped: the writer is still appending while the panel polls.
        let mut tail = assistant_call("t2", "w2", r#"{"path":"b.rs","content":"two\n"}"#, "write");
        tail.push_str(&tool_result("w2", "write", false));
        let cut = tail.len() - 12;
        fs::write(&path, format!("{head}{}", &tail[..cut])).unwrap();
        assert!(!scan.poll(&path));
        assert_eq!(scan.files().len(), 1);

        fs::write(&path, format!("{head}{tail}")).unwrap();
        assert!(scan.poll(&path));
        assert!(scan.version() > v1);
        assert_eq!(scan.files().len(), 2);
    }

    fn assistant_call(ts: &str, id: &str, args: &str, name: &str) -> String {
        format!(
            r#"{{"type":"message","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"{id}","name":"{name}","arguments":{args}}}]}}}}"#
        ) + "\n"
    }

    fn tool_result(id: &str, name: &str, is_error: bool) -> String {
        format!(
            r#"{{"type":"message","timestamp":"z","message":{{"role":"toolResult","toolCallId":"{id}","toolName":"{name}","isError":{is_error},"content":[{{"type":"text","text":"ok"}}]}}}}"#
        ) + "\n"
    }

    /// Build a session from `(id, tool, args)` triples, each with a successful
    /// result, and scan it with `cwd`.
    fn scan_calls(cwd: &Path, calls: &[(&str, &str, &str)]) -> ModifiedFilesScan {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut body = String::new();
        for (i, (id, name, args)) in calls.iter().enumerate() {
            body.push_str(&assistant_call(&format!("t{i}"), id, args, name));
            body.push_str(&tool_result(id, name, false));
        }
        fs::write(&path, body).unwrap();
        let mut scan = ModifiedFilesScan::new(cwd);
        scan.poll(&path);
        scan
    }

    #[test]
    fn short_patch_path_folds_onto_the_known_full_path() {
        let cwd = Path::new("/w");
        // write records the full path, a later patch header only a fragment.
        let scan = scan_calls(
            cwd,
            &[
                ("w1", "write", r#"{"path":"src/deep/a.rs","content":"one\n"}"#),
                ("e1", "edit", r#"{"input":"[a.rs#1A]\nCUT 1"}"#),
                ("e2", "edit", r#"{"input":"[deep/a.rs#1B]\nCUT 2"}"#),
            ],
        );
        assert_eq!(scan.files().len(), 1);
        assert_eq!(scan.files()[0].path, "src/deep/a.rs");
        assert_eq!(scan.files()[0].count, 3);

        // Other order: short rows are absorbed once the full path shows up.
        let scan = scan_calls(
            cwd,
            &[
                ("e1", "edit", r#"{"input":"[a.rs#1A]\nCUT 1"}"#),
                ("w1", "write", r#"{"path":"src/deep/a.rs","content":"one\n"}"#),
            ],
        );
        assert_eq!(scan.files().len(), 1);
        assert_eq!(scan.files()[0].path, "src/deep/a.rs");
        assert_eq!(scan.files()[0].count, 2);
    }

    #[test]
    fn full_write_paths_are_never_folded_into_each_other() {
        let scan = scan_calls(
            Path::new("/w"),
            &[
                ("w1", "write", r#"{"path":"x/src/a.rs","content":"one\n"}"#),
                ("w2", "write", r#"{"path":"src/a.rs","content":"two\n"}"#),
            ],
        );
        // `src/a.rs` is a real path, not a shorthand for `x/src/a.rs`.
        let mut names: Vec<&str> = scan.files().iter().map(|f| f.path.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["src/a.rs", "x/src/a.rs"]);
    }

    #[test]
    fn ambiguous_bare_name_stays_separate() {
        let scan = scan_calls(
            Path::new("/w"),
            &[
                ("w1", "write", r#"{"path":"x/a.rs","content":"one\n"}"#),
                ("w2", "write", r#"{"path":"y/a.rs","content":"two\n"}"#),
                ("e1", "edit", r#"{"input":"[a.rs#1A]\nCUT 1"}"#),
            ],
        );
        // Two candidates end in /a.rs, so folding would have to guess.
        let names: Vec<&str> = scan.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names.len(), 3, "got {names:?}");
        assert!(names.contains(&"a.rs"));
    }

    #[test]
    fn patch_header_variants() {
        assert_eq!(patch_header_path("[src/main.rs#C051]"), Some("src/main.rs"));
        assert_eq!(patch_header_path("[.zshrc#XXXX]"), Some(".zshrc"));
        assert_eq!(
            patch_header_path("[useChatSession.ts]"),
            Some("useChatSession.ts")
        );
        // A non-fingerprint suffix belongs to the path.
        assert_eq!(patch_header_path("[docs/a#section]"), Some("docs/a#section"));
        assert_eq!(patch_header_path("SWAP 3.=3:"), None);
        assert_eq!(patch_header_path("+[not a header]"), None);
    }

    #[test]
    fn path_normalization_folds_absolute_and_drops_uris() {
        let cwd = Path::new("/w/proj");
        assert_eq!(
            normalize_path("/w/proj/src/a.rs", cwd).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(normalize_path("./src/a.rs", cwd).as_deref(), Some("src/a.rs"));
        // Outside the session cwd: keep it absolute rather than guess.
        assert_eq!(
            normalize_path("/etc/hosts", cwd).as_deref(),
            Some("/etc/hosts")
        );
        assert_eq!(normalize_path("xd://mcp__noop", cwd), None);
        assert_eq!(normalize_path("  ", cwd), None);
    }

    /// End-to-end against a real omp session JSONL. Ignored by default; run with:
    ///   AMUX_TEST_SESSION=/path/to/session.jsonl \
    ///     cargo test --lib -- --ignored scans_real_session --nocapture
    #[test]
    #[ignore]
    fn scans_real_session() {
        let Ok(raw) = std::env::var("AMUX_TEST_SESSION") else {
            eprintln!("skipped: set AMUX_TEST_SESSION to a session jsonl");
            return;
        };
        let path = PathBuf::from(raw);
        let cwd = std::env::var("AMUX_TEST_CWD").unwrap_or_else(|_| "/".to_string());
        let mut scan = ModifiedFilesScan::new(Path::new(&cwd));
        let t0 = std::time::Instant::now();
        scan.poll(&path);
        let first = t0.elapsed();
        let t1 = std::time::Instant::now();
        scan.poll(&path);
        println!(
            "{} bytes: first scan {:?}, no-op poll {:?}",
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            first,
            t1.elapsed()
        );
        let rendered: usize = (0..scan.files().len())
            .map(|i| {
                scan.file_diff(i)
                    .iter()
                    .map(|l| l.text.len() + 1)
                    .sum::<usize>()
            })
            .sum();
        println!("rendered diff bytes across all files: {rendered}");
        // A session may legitimately have none (e.g. every `write` was an
        // xd:// MCP call), so only the shape of what we found is asserted.
        println!("{} files", scan.files().len());
        for f in scan.files().iter().take(12) {
            println!(
                "{:<50} ×{} {} {}",
                f.path,
                f.count,
                f.last_op.as_str(),
                f.last_time.as_deref().unwrap_or("-"),
            );
        }
        assert!(
            scan.files().iter().all(|f| !f.path.contains("://")),
            "internal URI leaked: {:?}",
            scan.files().iter().find(|f| f.path.contains("://"))
        );
    }
}
