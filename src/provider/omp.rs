//! OmpProvider: list disk sessions + build spawn/resume argv.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

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

/// Encode cwd the way omp layouts `~/.omp/agent/sessions/<key>/`.
pub fn encode_cwd_key(cwd: &Path) -> String {
    let abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let abs_str = abs.to_string_lossy();
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = abs.strip_prefix(&home) {
            let enc = rel
                .to_string_lossy()
                .replace('/', "-")
                .replace('\\', "-");
            return format!("-{enc}");
        }
    }
    abs_str.replace('/', "-").replace('\\', "-")
}

pub fn list_omp_sessions(sessions_root: &Path, cwd: &Path) -> Result<Vec<OmpDiskSession>> {
    let key = encode_cwd_key(cwd);
    let dir = sessions_root.join(&key);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let (id, jsonl_path) = if name.ends_with(".jsonl") {
            let stem = name.trim_end_matches(".jsonl");
            let id = extract_session_id(stem);
            (id, path.clone())
        } else if path.is_dir() {
            let jsonl = dir.join(format!("{name}.jsonl"));
            if jsonl.exists() {
                continue;
            }
            (extract_session_id(&name), path.clone())
        } else {
            continue;
        };

        if let Some(sess) = read_disk_session(id, &jsonl_path) {
            out.push(sess);
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
pub fn delete_session_with_artifacts(session_path: &Path) -> Result<()> {
    let Some(artifacts) = session_artifacts_dir(session_path) else {
        bail!("not an omp session jsonl: {}", session_path.display());
    };
    if session_path.exists() {
        fs::remove_file(session_path).with_context(|| {
            format!("remove session file {}", session_path.display())
        })?;
    }
    if artifacts.exists() {
        fs::remove_dir_all(&artifacts).with_context(|| {
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
    use std::io::Write;

    fn write_title_slot(path: &Path, title: &str) {
        for pad_len in 0..TITLE_SLOT_BYTES {
            let obj = serde_json::json!({
                "type": "title",
                "v": 1,
                "title": title,
                "updatedAt": "2026-08-01T00:00:00.000Z",
                "pad": " ".repeat(pad_len),
            });
            let mut out = serde_json::to_string(&obj).unwrap().into_bytes();
            out.push(b'\n');
            if out.len() == TITLE_SLOT_BYTES {
                File::create(path).unwrap().write_all(&out).unwrap();
                return;
            }
        }
        panic!("unable to build {TITLE_SLOT_BYTES}-byte title slot");
    }

    #[test]
    fn encode_home_relative() {
        let home = dirs::home_dir().unwrap();
        let cwd = home.join("projects/my-app");
        let key = encode_cwd_key(&cwd);
        assert!(key.starts_with('-'), "{key}");
        assert!(!key.contains('/'), "{key}");
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
}
