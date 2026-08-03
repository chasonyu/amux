//! OmpProvider: list disk sessions + build spawn/resume argv.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::AmuxConfig;

/// omp JSONL first-line title slot (including trailing newline).
pub const TITLE_SLOT_BYTES: usize = 256;
const FIRST_MSG_MAX_BYTES: u64 = 64 * 1024;
const FIRST_MSG_MAX_LINES: usize = 200;
const PROVISIONAL_MAX_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleKind {
    /// Non-empty `type:"title"` slot (LLM auto or `/rename`).
    Official,
    /// First user message while slot still empty.
    Provisional,
    /// Synthetic / id fallback.
    Fallback,
}

#[derive(Debug, Clone)]
pub struct OmpDiskSession {
    pub id: String,
    pub title: String,
    pub title_kind: TitleKind,
    /// omp `header.parentSession` — uuid (fork) or source file path (branch).
    pub parent_session: Option<String>,
    pub path: PathBuf,
    pub mtime: DateTime<Utc>,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
struct TitleLine {
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionHeaderLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "parentSession")]
    parent_session: Option<String>,
}

/// Whether an omp `parentSession` value refers to `session_id` (uuid or path).
pub fn parent_refers_to(parent: &str, session_id: &str) -> bool {
    if parent == session_id {
        return true;
    }
    // branch stores absolute path: `.../{ts}_{uuid}.jsonl`
    if parent.contains(session_id) {
        return true;
    }
    false
}

pub struct OmpProvider {
    pub omp_bin: String,
    pub session_dir_override: Option<PathBuf>,
    pub profile: Option<String>,
}

impl OmpProvider {
    pub fn from_config(cfg: &AmuxConfig) -> Self {
        Self {
            omp_bin: cfg.omp_command(),
            session_dir_override: cfg.session_dir.as_ref().map(PathBuf::from),
            profile: cfg.profile.clone(),
        }
    }

    pub fn sessions_root(&self) -> PathBuf {
        if let Some(ref d) = self.session_dir_override {
            return d.clone();
        }
        if let Some(ref profile) = self.profile {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let candidate = home.join(format!(".omp-{profile}/agent/sessions"));
            if candidate.is_dir() {
                return candidate;
            }
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".omp/agent/sessions")
    }

    pub fn session_dir_for_cwd(&self, cwd: &Path) -> PathBuf {
        self.sessions_root().join(encode_cwd_key(cwd))
    }

    pub fn list_sessions(&self, cwd: &Path) -> Result<Vec<OmpDiskSession>> {
        list_omp_sessions(&self.sessions_root(), cwd)
    }

    /// `omp --cwd <ws>` (+ optional profile / session-dir)
    pub fn spawn_new_args(&self, cwd: &Path) -> Vec<String> {
        let mut args = vec!["--cwd".into(), cwd.to_string_lossy().into_owned()];
        self.append_common(&mut args);
        args
    }

    /// `omp --cwd <ws> --resume <id>`
    pub fn spawn_resume_args(&self, cwd: &Path, session_id: &str) -> Vec<String> {
        let mut args = vec![
            "--cwd".into(),
            cwd.to_string_lossy().into_owned(),
            "--resume".into(),
            session_id.to_string(),
        ];
        self.append_common(&mut args);
        args
    }

    fn append_common(&self, args: &mut Vec<String>) {
        if let Some(ref p) = self.profile {
            args.push("--profile".into());
            args.push(p.clone());
        }
        if let Some(ref d) = self.session_dir_override {
            args.push("--session-dir".into());
            args.push(d.to_string_lossy().into_owned());
        }
    }

    pub fn omp_available(&self) -> bool {
        Path::new(&self.omp_bin).is_file() || which::which_like(&self.omp_bin)
    }
}

/// Primary omp v17.2.5+ session bucket for `cwd` (`home-{name}-{sha256}`).
pub fn encode_cwd_key(cwd: &Path) -> String {
    session_dir_names(cwd).primary
}

#[derive(Debug, Clone)]
struct SessionDirNames {
    primary: String,
    legacy_relative: Option<String>,
    legacy_absolute: String,
}

/// Match omp `getDefaultSessionDirName` (v17.2.5+), plus legacy bucket names.
fn session_dir_names(cwd: &Path) -> SessionDirNames {
    let resolved = canonical_cwd(cwd);
    let normalized = resolved.to_string_lossy().replace('\\', "/");

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let canonical_home = canonical_cwd(&home);
    let temp = std::env::temp_dir();
    let canonical_temp = canonical_cwd(&temp);

    let (scope, legacy_relative) = if resolved.starts_with(&canonical_home) {
        let rel = resolved.strip_prefix(&canonical_home).unwrap_or(Path::new(""));
        ("home", Some(encode_legacy_relative("-", rel)))
    } else if resolved.starts_with(&canonical_temp) {
        let rel = resolved.strip_prefix(&canonical_temp).unwrap_or(Path::new(""));
        ("tmp", Some(encode_legacy_relative("-tmp", rel)))
    } else {
        ("abs", None)
    };

    let readable = readable_basename(&resolved);
    let digest = sha256_hex(&normalized);
    let primary = format!(
        "{scope}-{}-{digest}",
        if readable.is_empty() { "project" } else { &readable }
    );
    let legacy_absolute = encode_legacy_absolute(&resolved);

    SessionDirNames {
        primary,
        legacy_relative,
        legacy_absolute,
    }
}

fn canonical_cwd(cwd: &Path) -> PathBuf {
    cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())
}

fn encode_legacy_relative(prefix: &str, relative: &Path) -> String {
    let encoded = relative
        .to_string_lossy()
        .replace(['/', '\\', ':'], "-");
    if encoded.is_empty() {
        prefix.to_string()
    } else if prefix.ends_with('-') {
        format!("{prefix}{encoded}")
    } else {
        format!("{prefix}-{encoded}")
    }
}

fn encode_legacy_absolute(resolved: &Path) -> String {
    let lossy = resolved.to_string_lossy();
    let trimmed = lossy.trim_start_matches(['/', '\\']);
    let encoded = trimmed.replace(['/', '\\', ':'], "-");
    format!("--{encoded}--")
}

fn readable_basename(resolved: &Path) -> String {
    let base = resolved
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let mut out = String::new();
    let mut prev_dash = false;
    for c in base.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        if ok {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 80 {
        trimmed.to_string()
    } else {
        chars[chars.len() - 80..].iter().collect()
    }
}

fn sha256_hex(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn session_dir_candidates(cwd: &Path) -> Vec<String> {
    let names = session_dir_names(cwd);
    let mut keys = vec![names.primary];
    if let Some(rel) = names.legacy_relative {
        if !keys.iter().any(|k| k == &rel) {
            keys.push(rel);
        }
    }
    if !keys.iter().any(|k| k == &names.legacy_absolute) {
        keys.push(names.legacy_absolute);
    }
    keys
}

pub fn list_omp_sessions(sessions_root: &Path, cwd: &Path) -> Result<Vec<OmpDiskSession>> {
    let mut out = Vec::new();
    let mut seen_paths = HashSet::new();
    for key in session_dir_candidates(cwd) {
        let dir = sessions_root.join(&key);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if !seen_paths.insert(path.clone()) {
                continue;
            }
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            // Only real session files. Orphan artifacts dirs (no sibling `.jsonl`)
            // used to be listed with Fallback title = uuid after a half-delete.
            if !name.ends_with(".jsonl") || !path.is_file() {
                continue;
            }
            let stem = name.trim_end_matches(".jsonl");
            let id = extract_session_id(stem);
            if let Some(sess) = read_disk_session(id, &path) {
                out.push(sess);
            }
        }
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    Ok(out)
}

/// Re-resolve display title for one jsonl (used by live watch refresh).
pub fn refresh_disk_session(path: &Path) -> Option<OmpDiskSession> {
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".jsonl") {
        return None;
    }
    let stem = name.trim_end_matches(".jsonl");
    let id = extract_session_id(stem);
    read_disk_session(id, path)
}

/// Sibling artifacts directory for a session jsonl (`foo.jsonl` → `foo/`).
/// Mirrors omp `FileSessionStorage.deleteSessionWithArtifacts`.
pub fn session_artifacts_dir(session_path: &Path) -> Option<PathBuf> {
    let s = session_path.to_str()?;
    if !s.ends_with(".jsonl") {
        return None;
    }
    Some(PathBuf::from(&s[..s.len() - ".jsonl".len()]))
}

/// Delete session jsonl and its sibling artifacts directory (best-effort on dir).
///
/// Retries once if a dying omp rewrite recreates the paths (common right after
/// kill). Prefer calling this only after the live PTY has been fully torn down.
pub fn delete_session_with_artifacts(session_path: &Path) -> Result<()> {
    let Some(artifacts) = session_artifacts_dir(session_path) else {
        bail!("not an omp session jsonl: {}", session_path.display());
    };
    remove_session_paths(session_path, &artifacts)?;
    // Dying omp may recreate an empty/partial session after the first unlink.
    if session_path.exists() || artifacts.exists() {
        std::thread::sleep(std::time::Duration::from_millis(200));
        remove_session_paths(session_path, &artifacts)?;
    }
    Ok(())
}

fn remove_session_paths(session_path: &Path, artifacts: &Path) -> Result<()> {
    if session_path.exists() {
        fs::remove_file(session_path).with_context(|| {
            format!("remove session file {}", session_path.display())
        })?;
    }
    if artifacts.exists() {
        fs::remove_dir_all(artifacts).with_context(|| {
            format!("remove artifacts dir {}", artifacts.display())
        })?;
    }
    Ok(())
}

fn read_disk_session(id: String, jsonl_path: &Path) -> Option<OmpDiskSession> {
    let meta = fs::metadata(jsonl_path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .map(system_time_to_utc)
        .unwrap_or_else(Utc::now);
    let size = meta.len();
    let (title, title_kind) = resolve_display_title(jsonl_path, &id);
    let parent_session = read_parent_session(jsonl_path);
    Some(OmpDiskSession {
        id,
        title,
        title_kind,
        parent_session,
        path: jsonl_path.to_path_buf(),
        mtime,
        size,
    })
}

fn read_parent_session(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    skip_title_prefix(&mut f).ok()?;
    let mut line = String::new();
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(f);
    reader.read_line(&mut line).ok()?;
    let header: SessionHeaderLine = serde_json::from_str(line.trim()).ok()?;
    if header.kind != "session" {
        return None;
    }
    header
        .parent_session
        .filter(|p| !p.trim().is_empty())
}

/// Official slot → first user message → fallback id.
pub fn resolve_display_title(path: &Path, fallback_id: &str) -> (String, TitleKind) {
    if let Some(t) = read_official_title(path) {
        return (t, TitleKind::Official);
    }
    if let Some(t) = read_first_user_message(path) {
        return (truncate_chars(&t, PROVISIONAL_MAX_CHARS), TitleKind::Provisional);
    }
    (fallback_id.to_string(), TitleKind::Fallback)
}

/// Sanitize a user-facing session title (first line, strip controls, trim).
pub fn sanitize_session_title(value: &str) -> Option<String> {
    sanitize_session_name(Some(value))
}

/// Persist an official title into omp's fixed 256-byte JSONL title slot (`source: user`).
///
/// - Files that already have a fixed-width title slot: overwrite the first 256 bytes in place.
/// - Legacy files (variable first line / no slot): prepend a fixed slot and rewrite the body.
pub fn write_session_title(path: &Path, title: &str) -> Result<()> {
    let title = sanitize_session_name(Some(title)).context("empty session title")?;
    let updated_at = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let slot = serialize_title_slot(&title, Some("user"), &updated_at)
        .context("serialize title slot")?;

    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut head = [0u8; TITLE_SLOT_BYTES];
    let n = file.read(&mut head)?;

    if n >= TITLE_SLOT_BYTES && is_fixed_title_slot(&head) {
        let mut out = OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("rewrite title slot {}", path.display()))?;
        out.write_all(&slot)?;
        out.flush()?;
        return Ok(());
    }

    // Legacy / short header: keep body after the first line (or whole file if no title line).
    let mut rest = Vec::new();
    if n == 0 {
        bail!("session file is empty: {}", path.display());
    }
    let text = std::str::from_utf8(&head[..n]).context("session file head is not utf-8")?;
    let first = text.split('\n').next().unwrap_or("");
    let skip_first = serde_json::from_str::<TitleLine>(first.trim_end())
        .ok()
        .is_some_and(|v| v.kind == "title");
    if skip_first {
        if let Some(idx) = head[..n].iter().position(|&b| b == b'\n') {
            rest.extend_from_slice(&head[idx + 1..n]);
        }
        // else: title line without newline in head — drop the partial head
    } else {
        rest.extend_from_slice(&head[..n]);
    }
    file.read_to_end(&mut rest)?;

    let tmp = path.with_extension("jsonl.amux-title-tmp");
    {
        let mut out = File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        out.write_all(&slot)?;
        out.write_all(&rest)?;
        out.flush()?;
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "replace {} with titled body ({})",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn is_fixed_title_slot(head: &[u8; TITLE_SLOT_BYTES]) -> bool {
    if head[TITLE_SLOT_BYTES - 1] != b'\n' {
        return false;
    }
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    let line = text.trim_end_matches('\n');
    serde_json::from_str::<TitleLine>(line)
        .ok()
        .is_some_and(|v| v.kind == "title")
}

/// Build omp's fixed-width title slot (exactly [`TITLE_SLOT_BYTES`] including trailing newline).
fn serialize_title_slot(
    title: &str,
    source: Option<&str>,
    updated_at: &str,
) -> Result<Vec<u8>> {
    let truncated = truncate_title_for_slot(title, source, updated_at)?;
    let unpadded = title_slot_line(&truncated, source, updated_at, "");
    let unpadded_len = unpadded.len();
    if unpadded_len > TITLE_SLOT_BYTES {
        bail!("title slot metadata exceeds {TITLE_SLOT_BYTES} bytes");
    }
    let pad_len = TITLE_SLOT_BYTES - unpadded_len;
    let line = title_slot_line(&truncated, source, updated_at, &" ".repeat(pad_len));
    let bytes = line.into_bytes();
    if bytes.len() != TITLE_SLOT_BYTES {
        bail!(
            "title slot length {} != {TITLE_SLOT_BYTES}",
            bytes.len()
        );
    }
    Ok(bytes)
}

fn title_slot_line(title: &str, source: Option<&str>, updated_at: &str, pad: &str) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), Value::String("title".into()));
    obj.insert("v".into(), Value::from(1));
    obj.insert("title".into(), Value::String(title.to_string()));
    if let Some(src) = source {
        obj.insert("source".into(), Value::String(src.to_string()));
    }
    obj.insert("updatedAt".into(), Value::String(updated_at.to_string()));
    obj.insert("pad".into(), Value::String(pad.to_string()));
    format!("{}\n", Value::Object(obj))
}

fn truncate_title_for_slot(
    title: &str,
    source: Option<&str>,
    updated_at: &str,
) -> Result<String> {
    let chars: Vec<char> = title.chars().collect();
    let mut low = 0usize;
    let mut high = chars.len();
    let mut best = String::new();
    while low <= high {
        let mid = (low + high) / 2;
        let candidate: String = chars[..mid].iter().collect();
        let line = title_slot_line(&candidate, source, updated_at, "");
        if line.len() <= TITLE_SLOT_BYTES {
            best = candidate;
            low = mid + 1;
        } else if mid == 0 {
            bail!("title slot metadata exceeds fixed slot size");
        } else {
            high = mid - 1;
        }
    }
    Ok(best)
}

fn read_official_title(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; TITLE_SLOT_BYTES];
    let n = f.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let text = std::str::from_utf8(&buf[..n]).ok()?;
    let line = text.split('\n').next().unwrap_or("");
    let v: TitleLine = serde_json::from_str(line.trim_end()).ok()?;
    if v.kind != "title" {
        return None;
    }
    sanitize_session_name(v.title.as_deref())
}

fn read_first_user_message(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    skip_title_prefix(&mut f).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        let take = (FIRST_MSG_MAX_BYTES.saturating_sub(total)).min(n as u64) as usize;
        buf.extend_from_slice(&chunk[..take]);
        total += take as u64;
        if total >= FIRST_MSG_MAX_BYTES {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = 0usize;
    for line in text.lines() {
        lines += 1;
        if lines > FIRST_MSG_MAX_LINES {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let message = v.get("message")?;
        if message.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let content = message.get("content")?;
        let raw = extract_text_content(content)?;
        if let Some(s) = sanitize_session_name(Some(&raw)) {
            return Some(s);
        }
    }
    None
}

/// Advance past omp's fixed title slot (or a legacy first-line title entry).
pub(crate) fn skip_title_prefix(f: &mut File) -> std::io::Result<()> {
    let mut head = [0u8; TITLE_SLOT_BYTES];
    let n = f.read(&mut head)?;
    if n == 0 {
        return Ok(());
    }
    let Ok(text) = std::str::from_utf8(&head[..n]) else {
        f.seek(SeekFrom::Start(0))?;
        return Ok(());
    };
    let line = text.split('\n').next().unwrap_or("");
    if serde_json::from_str::<TitleLine>(line.trim_end())
        .ok()
        .is_some_and(|v| v.kind == "title")
    {
        if let Some(idx) = head[..n].iter().position(|&b| b == b'\n') {
            f.seek(SeekFrom::Start((idx + 1) as u64))?;
            return Ok(());
        }
        // Full 256-byte slot without finding newline in the buffer — treat as slot.
        if n >= TITLE_SLOT_BYTES {
            f.seek(SeekFrom::Start(TITLE_SLOT_BYTES as u64))?;
            return Ok(());
        }
    }
    f.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn extract_text_content(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push(' ');
                        }
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

fn sanitize_session_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let first_line = value.lines().next().unwrap_or("");
    let stripped: String = first_line
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
}

fn extract_session_id(stem: &str) -> String {
    if let Some(pos) = stem.rfind('_') {
        let maybe = &stem[pos + 1..];
        if maybe.len() >= 8 {
            return maybe.to_string();
        }
    }
    stem.to_string()
}

fn system_time_to_utc(t: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// Tiny which without extra crate.
mod which {
    use std::path::Path;
    pub fn which_like(bin: &str) -> bool {
        if Path::new(bin).is_file() {
            return true;
        }
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        for dir in std::env::split_paths(&path) {
            if dir.join(bin).is_file() {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn write_title_slot(path: &Path, title: &str) {
        let slot = serialize_title_slot(title, None, "2026-08-01T00:00:00.000Z").unwrap();
        File::create(path).unwrap().write_all(&slot).unwrap();
    }

    #[test]
    fn encode_home_relative() {
        let home = dirs::home_dir().unwrap();
        let cwd = home.join("projects/my-app");
        let key = encode_cwd_key(&cwd);
        assert!(key.starts_with("home-"), "{key}");
        assert!(!key.contains('/'), "{key}");
        let digest = key.rsplit('-').next().unwrap();
        assert_eq!(digest.len(), 64, "{key}");
    }

    #[test]
    fn encode_matches_omp_v17_known_hash() {
        let normalized = "/home/user/projects/demo";
        assert_eq!(
            sha256_hex(normalized),
            "6285a494fa382fda51d6fccef028996d8039b2e74dc40893851588e1402d1da8"
        );
        let cwd = PathBuf::from(normalized);
        if cwd.exists() {
            let key = encode_cwd_key(&cwd);
            assert_eq!(
                key,
                "home-demo-6285a494fa382fda51d6fccef028996d8039b2e74dc40893851588e1402d1da8"
            );
        }
    }

    #[test]
    fn list_merges_legacy_session_dir() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let legacy = session_dir_names(cwd.path())
            .legacy_relative
            .expect("tempdir under system temp");
        let sess_dir = sessions_root.path().join(&legacy);
        fs::create_dir_all(&sess_dir).unwrap();

        let real = sess_dir.join("2026-08-01T00-00-00-000Z_realid.jsonl");
        write_title_slot(&real, "Legacy Bucket");
        let mut f = File::options().append(true).open(&real).unwrap();
        writeln!(f, r#"{{"type":"session","id":"realid"}}"#).unwrap();

        let list = list_omp_sessions(sessions_root.path(), cwd.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Legacy Bucket");
    }

    #[test]
    fn extract_id_from_stem() {
        let id = extract_session_id(
            "2026-07-27T02-58-31-899Z_019fa182-7f5b-7000-a63a-2352a49dbca2",
        );
        assert_eq!(id, "019fa182-7f5b-7000-a63a-2352a49dbca2");
    }

    #[test]
    fn resolve_official_from_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_title_slot(&path, "你好世界");
        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(f, r#"{{"type":"session","id":"abc"}}"#).unwrap();

        let (title, kind) = resolve_display_title(&path, "abc");
        assert_eq!(kind, TitleKind::Official);
        assert_eq!(title, "你好世界");
    }

    #[test]
    fn write_session_title_overlays_fixed_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_title_slot(&path, "old");
        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(f, r#"{{"type":"session","id":"abc"}}"#).unwrap();
        let before = fs::metadata(&path).unwrap().len();

        write_session_title(&path, "新名字").unwrap();
        let after = fs::metadata(&path).unwrap().len();
        assert_eq!(before, after, "in-place slot write must not change file length");

        let (title, kind) = resolve_display_title(&path, "abc");
        assert_eq!(kind, TitleKind::Official);
        assert_eq!(title, "新名字");

        let mut head = vec![0u8; TITLE_SLOT_BYTES];
        File::open(&path).unwrap().read_exact(&mut head).unwrap();
        let line = std::str::from_utf8(&head).unwrap().trim_end_matches('\n');
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v.get("source").and_then(|s| s.as_str()), Some("user"));
    }

    #[test]
    fn write_session_title_prepends_legacy_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        fs::write(
            &path,
            format!("{}\n", r#"{"type":"session","id":"abc"}"#),
        )
        .unwrap();

        write_session_title(&path, "legacy rename").unwrap();
        let (title, kind) = resolve_display_title(&path, "abc");
        assert_eq!(kind, TitleKind::Official);
        assert_eq!(title, "legacy rename");

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains(r#""type":"session""#), "{body}");
    }

    #[test]
    fn parent_refers_to_uuid_and_path() {
        let id = "019fbc08-4aff-7000-ba28-7e6c08ce05e7";
        assert!(parent_refers_to(id, id));
        assert!(parent_refers_to(
            &format!("/home/admin/.omp/agent/sessions/x/2026-08-01T00-00-00-000Z_{id}.jsonl"),
            id
        ));
        assert!(!parent_refers_to("019fbc08-4aff-7000-ba28-7e6c08ce05e8", id));
    }

    #[test]
    fn resolve_provisional_from_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_title_slot(&path, "");
        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"user","content":[{{"type":"text","text":"keep going please"}}]}}}}"#
        )
        .unwrap();

        let (title, kind) = resolve_display_title(&path, "abc");
        assert_eq!(kind, TitleKind::Provisional);
        assert_eq!(title, "keep going please");
    }

    #[test]
    fn delete_session_removes_jsonl_and_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("2026-08-01T00-00-00-000Z_abc.jsonl");
        let artifacts = dir.path().join("2026-08-01T00-00-00-000Z_abc");
        fs::write(&jsonl, b"{}\n").unwrap();
        fs::create_dir_all(artifacts.join("nested")).unwrap();
        fs::write(artifacts.join("nested/x.bin"), b"x").unwrap();

        assert_eq!(
            session_artifacts_dir(&jsonl).as_deref(),
            Some(artifacts.as_path())
        );
        delete_session_with_artifacts(&jsonl).unwrap();
        assert!(!jsonl.exists());
        assert!(!artifacts.exists());
    }

    #[test]
    fn list_skips_orphan_artifacts_dir_without_jsonl() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let key = encode_cwd_key(cwd.path());
        let sess_dir = sessions_root.path().join(&key);
        fs::create_dir_all(&sess_dir).unwrap();

        // Half-delete leftover: artifacts dir, no sibling jsonl.
        fs::create_dir_all(sess_dir.join("2026-08-01T00-00-00-000Z_orphanuuid")).unwrap();

        let real = sess_dir.join("2026-08-01T00-00-00-000Z_realid.jsonl");
        write_title_slot(&real, "Keep Me");
        let mut f = File::options().append(true).open(&real).unwrap();
        writeln!(f, r#"{{"type":"session","id":"realid"}}"#).unwrap();

        let list = list_omp_sessions(sessions_root.path(), cwd.path()).unwrap();
        assert_eq!(list.len(), 1, "orphan dir must not appear as a session");
        assert_eq!(list[0].title, "Keep Me");
        assert!(!list[0].id.contains("orphan"));
    }
}
