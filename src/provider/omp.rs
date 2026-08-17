//! OmpProvider: list disk sessions + build spawn/resume argv.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::AmuxConfig;
use crate::provider::api::{
    AgentProvider, LiveRenameAction, ModifiedFilesScanner, ProviderCapabilities, ProviderChange,
    ProviderId, ProviderSession, SessionKey, SpawnSpec, TitleSource,
};
use crate::provider::transcript::{DiffLine, ModifiedFile, ModifiedFilesScan, TranscriptBlock};
use crate::provider::turn_status::SessionActivityTracker;
use crate::provider::watch::{SessionDirEvent, SessionDirWatcher};

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
    /// PI_* env pins resolved from config (for SpawnSpec).
    pi_pins: Vec<(String, String)>,
    /// Moved from SessionSupervisor (§1.6).
    activity: SessionActivityTracker,
    /// Moved from Shell (§1.5).
    watcher: SessionDirWatcher,
    dirty_paths: HashSet<PathBuf>,
    debounce_until: Option<Instant>,
    need_rescan: bool,
    last_poll: Instant,
    /// Current watched workspace cwd (for fallback poll).
    watched_cwd: Option<PathBuf>,
    /// Last known session fingerprints for fallback poll.
    known_sessions: HashMap<PathBuf, (DateTime<Utc>, u64)>,
}

/// Title refresh debounce (§1.5: ~80ms).
const TITLE_WATCH_DEBOUNCE: Duration = Duration::from_millis(80);
/// Title fallback poll interval (§1.5: 3s).
const TITLE_FALLBACK_POLL: Duration = Duration::from_secs(3);

impl OmpProvider {
    pub fn from_config(cfg: &AmuxConfig) -> Self {
        Self {
            omp_bin: cfg.omp_command(),
            session_dir_override: cfg.session_dir.as_ref().map(PathBuf::from),
            profile: cfg.profile.clone(),
            pi_pins: cfg.effective_pi_pins(),
            activity: SessionActivityTracker::default(),
            watcher: SessionDirWatcher::spawn(),
            dirty_paths: HashSet::new(),
            debounce_until: None,
            need_rescan: false,
            last_poll: Instant::now(),
            watched_cwd: None,
            known_sessions: HashMap::new(),
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

/// Best-effort parse of `pgrep -af omp` output for an external resume of `session_id`.
fn parse_external_omp_resume(process_list: &str, session_id: &str) -> Option<String> {
    let prefix = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };
    for line in process_list.lines() {
        let lower = line.to_ascii_lowercase();
        let resume_hit =
            lower.contains("--resume") || lower.contains(" -r ") || lower.contains(" -r=");
        if resume_hit && line.contains(prefix) {
            if lower.contains("amux") {
                continue;
            }
            return Some(line.trim().to_string());
        }
    }
    None
}

// --- SPI helpers ---

fn title_kind_to_source(kind: TitleKind) -> TitleSource {
    match kind {
        TitleKind::Official => TitleSource::Official,
        TitleKind::Provisional => TitleSource::Provisional,
        TitleKind::Fallback => TitleSource::Fallback,
    }
}

/// Adapter wrapping [`ModifiedFilesScan`] as [`ModifiedFilesScanner`].
struct OmpModifiedFilesScanner {
    scan: ModifiedFilesScan,
}

impl ModifiedFilesScanner for OmpModifiedFilesScanner {
    fn advance(&mut self, session: &ProviderSession) -> Result<bool> {
        let path = session
            .path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("omp session has no path for modified-files scan"))?;
        Ok(self.scan.poll(path))
    }
    fn version(&self) -> u64 {
        self.scan.version()
    }
    fn files(&self) -> &[ModifiedFile] {
        self.scan.files()
    }
    fn render_diff(&self, file_index: usize) -> Vec<DiffLine> {
        self.scan.file_diff(file_index)
    }
}

impl AgentProvider for OmpProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OMP
    }

    fn display_name(&self) -> &'static str {
        "omp"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::OMP
    }

    fn available(&self) -> Result<()> {
        if self.omp_available() {
            Ok(())
        } else {
            bail!(
                "omp not found at '{}'. Install omp and ensure it is on PATH.",
                self.omp_bin
            )
        }
    }

    fn list_sessions(&mut self, cwd: &Path) -> Result<Vec<ProviderSession>> {
        let disk = list_omp_sessions(&self.sessions_root(), cwd)?;
        // Update known fingerprints for fallback poll.
        self.known_sessions.clear();
        for d in &disk {
            self.known_sessions
                .insert(d.path.clone(), (d.mtime, d.size));
        }
        Ok(disk
            .into_iter()
            .map(|d| ProviderSession {
                key: SessionKey::omp(&d.id),
                title: d.title,
                title_source: title_kind_to_source(d.title_kind),
                parent_ref: d.parent_session,
                path: Some(d.path),
                cwd: cwd.to_path_buf(),
                modified_at: d.mtime,
                size: d.size,
            })
            .collect())
    }

    fn spawn_new(&self, cwd: &Path) -> Result<SpawnSpec> {
        Ok(SpawnSpec {
            program: self.omp_bin.clone(),
            args: self.spawn_new_args(cwd),
            env: self.pi_pins.clone(),
            cwd: cwd.to_path_buf(),
        })
    }

    fn spawn_resume(&self, cwd: &Path, session_id: &str) -> Result<SpawnSpec> {
        Ok(SpawnSpec {
            program: self.omp_bin.clone(),
            args: self.spawn_resume_args(cwd, session_id),
            env: self.pi_pins.clone(),
            cwd: cwd.to_path_buf(),
        })
    }

    fn check_external_occupant(&self, session_id: &str) -> Result<()> {
        let process_list = match Command::new("pgrep").args(["-af", "omp"]).output() {
            Ok(output) if output.status.success() || !output.stdout.is_empty() => {
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            _ => return Ok(()),
        };
        if let Some(line) = parse_external_omp_resume(&process_list, session_id) {
            bail!(
                "session appears occupied by external omp:\n  {line}\n\
                 Attach refused (no force-hijack)."
            );
        }
        Ok(())
    }

    fn parent_refers_to(&self, parent_ref: &str, session_id: &str) -> bool {
        parent_refers_to(parent_ref, session_id)
    }

    fn session_busy(&mut self, session: &ProviderSession, live: bool, pty_active: bool) -> bool {
        self.activity.busy(
            &session.key.session_id,
            live,
            pty_active,
            session.path.as_deref(),
        )
    }

    fn forget_session(&mut self, key: &SessionKey) {
        self.activity.forget(&key.session_id);
    }

    fn normalize_title(&self, draft: &str) -> Result<String> {
        sanitize_session_title(draft).ok_or_else(|| anyhow::anyhow!("Title cannot be empty"))
    }

    fn rename_live(&mut self, _session: &ProviderSession, title: &str) -> Result<LiveRenameAction> {
        // Ctrl-U clears the omp editor line, then /rename + title + CR.
        let bytes = format!("\x15/rename {title}\r").into_bytes();
        Ok(LiveRenameAction::WritePty(bytes))
    }

    fn rename_stored(&mut self, session: &ProviderSession, title: &str) -> Result<()> {
        let path = session
            .path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("omp session has no path for stored rename"))?;
        write_session_title(path, title)
    }

    fn delete_stored(&mut self, session: &ProviderSession) -> Result<()> {
        let path = session
            .path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("omp session has no path for delete"))?;
        delete_session_with_artifacts(path)
    }

    fn select_workspace(&mut self, cwd: Option<&Path>) -> Result<()> {
        // Stop old watcher, discard old workspace queue.
        self.dirty_paths.clear();
        self.debounce_until = None;
        self.need_rescan = false;
        self.last_poll = Instant::now();
        self.known_sessions.clear();

        match cwd {
            Some(cwd) => {
                let dir = self.session_dir_for_cwd(cwd);
                self.watcher.set_dir(Some(dir));
                self.watched_cwd = Some(cwd.to_path_buf());
            }
            None => {
                self.watcher.set_dir(None);
                self.watched_cwd = None;
            }
        }
        Ok(())
    }

    fn poll_changes(&mut self, now: Instant) -> Result<Vec<ProviderChange>> {
        let mut changes = Vec::new();

        // 1. Drain watcher events.
        for ev in self.watcher.drain() {
            match ev {
                SessionDirEvent::Changed(p) | SessionDirEvent::Removed(p) => {
                    self.dirty_paths.insert(p);
                    self.debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
                }
                SessionDirEvent::Rescan => {
                    self.need_rescan = true;
                    self.debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
                }
            }
        }

        // 2. Debounce flush.
        if let Some(deadline) = self.debounce_until {
            if now >= deadline {
                self.debounce_until = None;
                if self.need_rescan {
                    self.need_rescan = false;
                    self.dirty_paths.clear();
                    changes.push(ProviderChange::Rescan);
                } else if !self.dirty_paths.is_empty() {
                    let paths: Vec<PathBuf> = self.dirty_paths.drain().collect();
                    for path in paths {
                        if path.exists() {
                            if let Some(disk) = refresh_disk_session(&path) {
                                let cwd = self
                                    .watched_cwd
                                    .clone()
                                    .unwrap_or_else(|| PathBuf::from("."));
                                changes.push(ProviderChange::Upsert(ProviderSession {
                                    key: SessionKey::omp(&disk.id),
                                    title: disk.title,
                                    title_source: title_kind_to_source(disk.title_kind),
                                    parent_ref: disk.parent_session,
                                    path: Some(disk.path),
                                    cwd,
                                    modified_at: disk.mtime,
                                    size: disk.size,
                                }));
                            }
                        } else {
                            // File removed — extract session id from stem.
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                let id = extract_session_id(stem);
                                changes.push(ProviderChange::Removed(SessionKey::omp(&id)));
                            }
                        }
                    }
                }
            }
        }

        // 3. Fallback poll (3s) — catches changes missed by the watcher,
        // including jsonl created in a legacy cwd bucket the primary watch
        // does not cover.
        if now.duration_since(self.last_poll) >= TITLE_FALLBACK_POLL {
            self.last_poll = now;
            if let Some(cwd) = self.watched_cwd.clone() {
                if let Ok(listed) = list_omp_sessions(&self.sessions_root(), &cwd) {
                    let listed_paths: HashSet<PathBuf> =
                        listed.iter().map(|d| d.path.clone()).collect();
                    let known_paths: HashSet<PathBuf> =
                        self.known_sessions.keys().cloned().collect();
                    if listed_paths != known_paths {
                        self.known_sessions
                            .retain(|path, _| listed_paths.contains(path));
                        for d in &listed {
                            self.known_sessions
                                .insert(d.path.clone(), (d.mtime, d.size));
                        }
                        changes.push(ProviderChange::Rescan);
                    } else {
                        for d in listed {
                            let changed = self
                                .known_sessions
                                .get(&d.path)
                                .is_none_or(|(mtime, size)| d.mtime != *mtime || d.size != *size);
                            if changed {
                                self.known_sessions
                                    .insert(d.path.clone(), (d.mtime, d.size));
                                changes.push(ProviderChange::Upsert(ProviderSession {
                                    key: SessionKey::omp(&d.id),
                                    title: d.title,
                                    title_source: title_kind_to_source(d.title_kind),
                                    parent_ref: d.parent_session,
                                    path: Some(d.path),
                                    cwd: cwd.clone(),
                                    modified_at: d.mtime,
                                    size: d.size,
                                }));
                            }
                        }
                    }
                }
            }
        }

        Ok(changes)
    }

    fn next_deadline(&self) -> Option<Instant> {
        // Earliest of debounce deadline and fallback poll deadline.
        let debounce = self.debounce_until;
        let fallback = Some(self.last_poll + TITLE_FALLBACK_POLL);
        match (debounce, fallback) {
            (Some(d), Some(f)) => Some(d.min(f)),
            (d, f) => d.or(f),
        }
    }

    fn load_transcript(&mut self, session: &ProviderSession) -> Result<Vec<TranscriptBlock>> {
        let path = session
            .path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("omp session has no path for transcript"))?;
        Ok(crate::provider::transcript::omp::load(path))
    }

    fn modified_files_scanner(
        &mut self,
        session: &ProviderSession,
    ) -> Result<Option<Box<dyn ModifiedFilesScanner>>> {
        Ok(Some(Box::new(OmpModifiedFilesScanner {
            scan: ModifiedFilesScan::new(&session.cwd),
        })))
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
        let rel = resolved
            .strip_prefix(&canonical_home)
            .unwrap_or(Path::new(""));
        ("home", Some(encode_legacy_relative("-", rel)))
    } else if resolved.starts_with(&canonical_temp) {
        let rel = resolved
            .strip_prefix(&canonical_temp)
            .unwrap_or(Path::new(""));
        ("tmp", Some(encode_legacy_relative("-tmp", rel)))
    } else {
        ("abs", None)
    };

    let readable = readable_basename(&resolved);
    let digest = sha256_hex(&normalized);
    let primary = format!(
        "{scope}-{}-{digest}",
        if readable.is_empty() {
            "project"
        } else {
            &readable
        }
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
    let encoded = relative.to_string_lossy().replace(['/', '\\', ':'], "-");
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
        fs::remove_file(session_path)
            .with_context(|| format!("remove session file {}", session_path.display()))?;
    }
    if artifacts.exists() {
        fs::remove_dir_all(artifacts)
            .with_context(|| format!("remove artifacts dir {}", artifacts.display()))?;
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
    header.parent_session.filter(|p| !p.trim().is_empty())
}

/// Official slot → first user message → fallback id.
pub fn resolve_display_title(path: &Path, fallback_id: &str) -> (String, TitleKind) {
    if let Some(t) = read_official_title(path) {
        return (t, TitleKind::Official);
    }
    if let Some(t) = read_first_user_message(path) {
        return (
            truncate_chars(&t, PROVISIONAL_MAX_CHARS),
            TitleKind::Provisional,
        );
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
    let slot =
        serialize_title_slot(&title, Some("user"), &updated_at).context("serialize title slot")?;

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
        let mut out = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
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
fn serialize_title_slot(title: &str, source: Option<&str>, updated_at: &str) -> Result<Vec<u8>> {
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
        bail!("title slot length {} != {TITLE_SLOT_BYTES}", bytes.len());
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

fn truncate_title_for_slot(title: &str, source: Option<&str>, updated_at: &str) -> Result<String> {
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
    let stripped: String = first_line.chars().filter(|c| !c.is_control()).collect();
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
    format!(
        "{}…",
        s.chars().take(max.saturating_sub(1)).collect::<String>()
    )
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
    use crate::provider::api::{AgentProvider, ProviderChange, ProviderId};
    use crate::provider::registry::ProviderRegistry;
    use crate::session::{SessionSummaryChange, SessionSupervisor};
    use std::io::{Read, Write};
    use std::time::Duration;

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
        let id =
            extract_session_id("2026-07-27T02-58-31-899Z_019fa182-7f5b-7000-a63a-2352a49dbca2");
        assert_eq!(id, "019fa182-7f5b-7000-a63a-2352a49dbca2");
        assert_eq!(extract_session_id("just-an-id"), "just-an-id");
        assert_eq!(extract_session_id("short_ab"), "short_ab");
        assert_eq!(extract_session_id("nounderscore"), "nounderscore");
        assert_eq!(
            extract_session_id("prefix_0123456789abcdef"),
            "0123456789abcdef"
        );
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
        assert_eq!(
            before, after,
            "in-place slot write must not change file length"
        );

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
        fs::write(&path, format!("{}\n", r#"{"type":"session","id":"abc"}"#)).unwrap();

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
            &format!("/home/user/.omp/agent/sessions/x/2026-08-01T00-00-00-000Z_{id}.jsonl"),
            id
        ));
        assert!(!parent_refers_to(
            "019fbc08-4aff-7000-ba28-7e6c08ce05e8",
            id
        ));
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

    // --- SPI contract tests (§4.4) ---

    fn make_provider() -> OmpProvider {
        OmpProvider {
            omp_bin: "omp".to_string(),
            session_dir_override: None,
            profile: None,
            pi_pins: vec![
                ("PI_FORCE_IMAGE_PROTOCOL".into(), "off".into()),
                ("PI_NO_DECCARA".into(), "1".into()),
                ("PI_NO_KITTY_PLACEHOLDERS".into(), "1".into()),
                ("PI_TUI_SYNC_OUTPUT".into(), "1".into()),
            ],
            activity: SessionActivityTracker::default(),
            watcher: SessionDirWatcher::spawn(),
            dirty_paths: HashSet::new(),
            debounce_until: None,
            need_rescan: false,
            last_poll: Instant::now(),
            watched_cwd: None,
            known_sessions: HashMap::new(),
        }
    }

    #[test]
    fn omp_new_spawn_spec_matches_legacy_argv() {
        let p = make_provider();
        let cwd = Path::new("/tmp/ws");
        let spec = p.spawn_new(cwd).unwrap();
        assert_eq!(spec.program, "omp");
        assert_eq!(spec.args, vec!["--cwd", "/tmp/ws"]);
        assert_eq!(spec.cwd, cwd);
    }

    #[test]
    fn omp_resume_spawn_spec_matches_legacy_argv() {
        let p = make_provider();
        let cwd = Path::new("/tmp/ws");
        let spec = p.spawn_resume(cwd, "abc-123").unwrap();
        assert_eq!(spec.program, "omp");
        assert_eq!(spec.args, vec!["--cwd", "/tmp/ws", "--resume", "abc-123"]);
        assert_eq!(spec.cwd, cwd);
    }

    #[test]
    fn omp_spawn_spec_preserves_default_pi_pins() {
        let p = make_provider();
        let spec = p.spawn_new(Path::new("/tmp/ws")).unwrap();
        assert_eq!(spec.env.len(), 4);
        let names: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"PI_FORCE_IMAGE_PROTOCOL"));
        assert!(names.contains(&"PI_NO_DECCARA"));
        assert!(names.contains(&"PI_NO_KITTY_PLACEHOLDERS"));
        assert!(names.contains(&"PI_TUI_SYNC_OUTPUT"));
        let vals: Vec<(&str, &str)> = spec
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert!(vals.contains(&("PI_FORCE_IMAGE_PROTOCOL", "off")));
        assert!(vals.contains(&("PI_NO_DECCARA", "1")));
        assert!(vals.contains(&("PI_NO_KITTY_PLACEHOLDERS", "1")));
        assert!(vals.contains(&("PI_TUI_SYNC_OUTPUT", "1")));
    }

    #[test]
    fn omp_spawn_spec_preserves_configured_pi_pins() {
        let mut p = make_provider();
        p.pi_pins = vec![("PI_CUSTOM".into(), "yes".into())];
        let spec = p.spawn_new(Path::new("/tmp/ws")).unwrap();
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.env[0].0, "PI_CUSTOM");
        assert_eq!(spec.env[0].1, "yes");
    }

    #[test]
    fn omp_spawn_spec_with_profile_and_session_dir() {
        let mut p = make_provider();
        p.profile = Some("dev".into());
        p.session_dir_override = Some(PathBuf::from("/custom/sessions"));
        let spec = p.spawn_new(Path::new("/tmp/ws")).unwrap();
        assert_eq!(
            spec.args,
            vec![
                "--cwd",
                "/tmp/ws",
                "--profile",
                "dev",
                "--session-dir",
                "/custom/sessions"
            ]
        );
        let resume = p.spawn_resume(Path::new("/tmp/ws"), "id1").unwrap();
        assert_eq!(
            resume.args,
            vec![
                "--cwd",
                "/tmp/ws",
                "--resume",
                "id1",
                "--profile",
                "dev",
                "--session-dir",
                "/custom/sessions"
            ]
        );
    }

    #[test]
    fn omp_live_rename_action_preserves_exact_bytes() {
        let mut p = make_provider();
        let session = ProviderSession {
            key: SessionKey::omp("test-id"),
            title: "old".into(),
            title_source: TitleSource::Fallback,
            parent_ref: None,
            path: Some(PathBuf::from("/tmp/test.jsonl")),
            cwd: PathBuf::from("/tmp"),
            modified_at: Utc::now(),
            size: 0,
        };
        let action = p.rename_live(&session, "new title").unwrap();
        match action {
            LiveRenameAction::WritePty(bytes) => {
                assert_eq!(bytes, b"\x15/rename new title\r");
            }
            LiveRenameAction::Persisted => panic!("expected WritePty for omp live rename"),
        }
    }

    #[test]
    fn omp_normalize_title_preserves_existing_rules() {
        let p = make_provider();
        // First line only
        assert_eq!(p.normalize_title("first\nsecond").unwrap(), "first");
        // Control chars stripped
        assert_eq!(p.normalize_title("hello\x07world").unwrap(), "helloworld");
        // Trim
        assert_eq!(p.normalize_title("  hello  ").unwrap(), "hello");
        // All-whitespace rejected
        assert!(p.normalize_title("   ").is_err());
        // Empty rejected
        assert!(p.normalize_title("").is_err());
        // None rejected
        assert!(p.normalize_title("\n\n").is_err());
    }

    #[test]
    fn omp_capabilities_are_all_true() {
        let p = make_provider();
        let caps = p.capabilities();
        assert!(caps.rename);
        assert!(caps.delete);
        assert!(caps.transcript);
        assert!(caps.modified_files);
        assert!(caps.live_rebind);
    }

    #[test]
    fn omp_provider_id_is_stable() {
        let p = make_provider();
        assert_eq!(p.id().as_str(), "omp");
        assert_eq!(p.display_name(), "omp");
    }

    fn write_official_jsonl(path: &Path, title: &str, parent: Option<&str>) {
        write_title_slot(path, title);
        let mut f = File::options().append(true).open(path).unwrap();
        match parent {
            Some(parent) => {
                writeln!(f, r#"{{"type":"session","parentSession":"{parent}"}}"#).unwrap()
            }
            None => writeln!(f, r#"{{"type":"session"}}"#).unwrap(),
        }
    }

    fn session_jsonl_path(sessions_root: &Path, cwd: &Path, id: &str) -> PathBuf {
        let key = encode_cwd_key(cwd);
        let sess_dir = sessions_root.join(key);
        fs::create_dir_all(&sess_dir).unwrap();
        sess_dir.join(format!("2026-08-01T00-00-00-000Z_{id}.jsonl"))
    }

    #[test]
    fn omp_maps_disk_session_without_losing_metadata() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let id = "019fa182-7f5b-7000-a63a-2352a49dbca2";
        let parent = "019fparent-0000-7000-ba28-7e6c08ce05e7";
        let path = session_jsonl_path(sessions_root.path(), cwd.path(), id);
        write_official_jsonl(&path, "Official Title", Some(parent));
        let meta = fs::metadata(&path).unwrap();
        let expected_mtime = system_time_to_utc(meta.modified().unwrap());
        let expected_size = meta.len();

        let mut p = make_provider();
        p.session_dir_override = Some(sessions_root.path().to_path_buf());
        let listed = AgentProvider::list_sessions(&mut p, cwd.path()).unwrap();
        assert_eq!(listed.len(), 1);
        let session = &listed[0];
        assert_eq!(session.key.session_id, id);
        assert_eq!(session.title, "Official Title");
        assert_eq!(session.title_source, TitleSource::Official);
        assert_eq!(session.parent_ref.as_deref(), Some(parent));
        assert_eq!(session.path.as_deref(), Some(path.as_path()));
        assert_eq!(session.cwd, cwd.path());
        assert_eq!(session.modified_at, expected_mtime);
        assert_eq!(session.size, expected_size);

        let spec = p.spawn_resume(cwd.path(), &session.key.session_id).unwrap();
        assert_eq!(spec.args[2], "--resume");
        assert_eq!(spec.args[3], id);

        assert!(p.modified_files_scanner(session).unwrap().is_some());
    }

    #[test]
    fn omp_stored_rename_preserves_title_slot_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_title_slot(&path, "old");
        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(f, r#"{{"type":"session","id":"abc"}}"#).unwrap();
        drop(f);
        let before = fs::metadata(&path).unwrap().len();

        let mut p = make_provider();
        let session = ProviderSession {
            key: SessionKey::omp("abc"),
            title: "old".into(),
            title_source: TitleSource::Official,
            parent_ref: None,
            path: Some(path.clone()),
            cwd: dir.path().to_path_buf(),
            modified_at: Utc::now(),
            size: before,
        };
        p.rename_stored(&session, "新名字").unwrap();
        let after = fs::metadata(&path).unwrap().len();
        assert_eq!(
            before, after,
            "in-place slot write must not change file length"
        );
        let (title, kind) = resolve_display_title(&path, "abc");
        assert_eq!(kind, TitleKind::Official);
        assert_eq!(title, "新名字");
    }

    #[test]
    fn omp_delete_preserves_artifact_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("2026-08-01T00-00-00-000Z_abc.jsonl");
        let artifacts = dir.path().join("2026-08-01T00-00-00-000Z_abc");
        fs::write(&jsonl, b"{}\n").unwrap();
        fs::create_dir_all(artifacts.join("nested")).unwrap();
        fs::write(artifacts.join("nested/x.bin"), b"x").unwrap();

        let mut p = make_provider();
        let session = ProviderSession {
            key: SessionKey::omp("abc"),
            title: "abc".into(),
            title_source: TitleSource::Fallback,
            parent_ref: None,
            path: Some(jsonl.clone()),
            cwd: dir.path().to_path_buf(),
            modified_at: Utc::now(),
            size: 3,
        };
        p.delete_stored(&session).unwrap();
        assert!(!jsonl.exists());
        assert!(!artifacts.exists());
    }

    #[test]
    fn omp_external_occupant_uses_resume_prefix_matching() {
        let id = "019fa182-7f5b-7000-a63a-2352a49dbca2";
        let cases: &[(&str, &str, &str, Option<&str>)] = &[
            (
                "--resume",
                "1234 omp --cwd /tmp --resume 019fa182-7f5b-7000-a63a-2352a49dbca2\n",
                id,
                Some("1234 omp --cwd /tmp --resume 019fa182-7f5b-7000-a63a-2352a49dbca2"),
            ),
            (
                "-r value",
                "1234 omp -r 019fa182-7f5b-7000-a63a-2352a49dbca2\n",
                id,
                Some("1234 omp -r 019fa182-7f5b-7000-a63a-2352a49dbca2"),
            ),
            (
                "-r=value",
                "1234 omp -r=019fa182-7f5b-7000-a63a-2352a49dbca2\n",
                id,
                Some("1234 omp -r=019fa182-7f5b-7000-a63a-2352a49dbca2"),
            ),
            (
                "different session no match",
                "1234 omp --resume ffffffff-1111-2222-3333-444444444444\n",
                id,
                None,
            ),
            (
                "amux own process line skipped",
                "999 /usr/bin/amux --resume 019fa182-7f5b-7000-a63a-2352a49dbca2\n",
                id,
                None,
            ),
            (
                "first valid match among multiple lines",
                "999 amux helper --resume 019fa182-7f5b-7000-a63a-2352a49dbca2\n\
                 1234 omp --resume 019fa182-7f5b-7000-a63a-2352a49dbca2\n\
                 5678 omp --resume 019fa182-7f5b-7000-a63a-2352a49dbca2\n",
                id,
                Some("1234 omp --resume 019fa182-7f5b-7000-a63a-2352a49dbca2"),
            ),
        ];
        for (name, process_list, session_id, expect) in cases {
            assert_eq!(
                parse_external_omp_resume(process_list, session_id).as_deref(),
                *expect,
                "{name}"
            );
        }
    }

    #[test]
    fn omp_watch_modify_emits_upsert_after_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let id = "019fa182-bbbb-7000-a63a-2352a49dbca2";
        let path = dir
            .path()
            .join(format!("2026-08-01T00-00-00-000Z_{id}.jsonl"));
        write_official_jsonl(&path, "Watched", None);

        let mut p = make_provider();
        let now = Instant::now();
        p.watched_cwd = Some(dir.path().to_path_buf());
        p.dirty_paths.insert(path);
        p.debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
        p.last_poll = now;
        p.need_rescan = false;

        assert!(p.poll_changes(now).unwrap().is_empty());
        let changes = p.poll_changes(now + TITLE_WATCH_DEBOUNCE).unwrap();
        match changes.as_slice() {
            [ProviderChange::Upsert(session)] => {
                assert_eq!(session.key.session_id, id);
            }
            other => panic!("expected one Upsert, got {other:?}"),
        }
    }

    #[test]
    fn omp_watch_create_delete_or_overflow_emits_rescan() {
        let mut p = make_provider();
        let now = Instant::now();
        p.need_rescan = true;
        p.debounce_until = Some(now);
        p.last_poll = now;
        let changes = p.poll_changes(now).unwrap();
        assert!(
            changes.iter().any(|c| matches!(c, ProviderChange::Rescan)),
            "expected Rescan, got {changes:?}"
        );
    }

    #[test]
    fn omp_watch_switch_workspace_drops_old_workspace_events() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd_a = tempfile::tempdir().unwrap();
        let cwd_b = tempfile::tempdir().unwrap();
        let id = "019fa182-cccc-7000-a63a-2352a49dbca2";
        let path_a = session_jsonl_path(sessions_root.path(), cwd_a.path(), id);
        write_official_jsonl(&path_a, "Workspace A", None);
        fs::create_dir_all(sessions_root.path().join(encode_cwd_key(cwd_b.path()))).unwrap();

        let mut p = make_provider();
        p.session_dir_override = Some(sessions_root.path().to_path_buf());
        p.select_workspace(Some(cwd_a.path())).unwrap();
        p.dirty_paths.insert(path_a);
        p.debounce_until = Some(Instant::now() + TITLE_WATCH_DEBOUNCE);
        p.need_rescan = true;

        p.select_workspace(Some(cwd_b.path())).unwrap();
        assert!(p.dirty_paths.is_empty());
        assert!(p.debounce_until.is_none());
        assert!(!p.need_rescan);

        let now = Instant::now();
        p.last_poll = now;
        let changes = p.poll_changes(now).unwrap();
        for change in &changes {
            if let ProviderChange::Upsert(session) = change {
                assert_ne!(
                    session.key.session_id, id,
                    "old workspace upsert leaked after switch"
                );
            }
        }
    }

    #[test]
    fn omp_watch_fallback_poll_detects_changed_fingerprint() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let id = "019fa182-dddd-7000-a63a-2352a49dbca2";
        let path = session_jsonl_path(sessions_root.path(), cwd.path(), id);
        write_official_jsonl(&path, "Before", None);

        let mut p = make_provider();
        p.session_dir_override = Some(sessions_root.path().to_path_buf());
        let now = Instant::now();
        p.watched_cwd = Some(cwd.path().to_path_buf());
        p.last_poll = now;
        let listed = AgentProvider::list_sessions(&mut p, cwd.path()).unwrap();
        assert_eq!(listed.len(), 1);
        let old_size = listed[0].size;

        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"user","content":"x"}}}}"#
        )
        .unwrap();
        drop(f);
        let new_size = fs::metadata(&path).unwrap().len();
        assert_ne!(old_size, new_size);

        let changes = p.poll_changes(now + Duration::from_secs(4)).unwrap();
        match changes.as_slice() {
            [ProviderChange::Upsert(session)] => {
                assert_eq!(session.key.session_id, id);
                assert_eq!(session.size, new_size);
            }
            other => panic!("expected fallback Upsert, got {other:?}"),
        }
    }

    #[test]
    fn omp_watch_fallback_poll_detects_new_session() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let first = "019fa182-dddd-7000-a63a-2352a49dbca2";
        let second = "019fa182-eeee-7000-a63a-2352a49dbca2";
        write_official_jsonl(
            &session_jsonl_path(sessions_root.path(), cwd.path(), first),
            "One",
            None,
        );

        let mut p = make_provider();
        p.session_dir_override = Some(sessions_root.path().to_path_buf());
        let now = Instant::now();
        p.watched_cwd = Some(cwd.path().to_path_buf());
        p.last_poll = now;
        assert_eq!(
            AgentProvider::list_sessions(&mut p, cwd.path())
                .unwrap()
                .len(),
            1
        );

        write_official_jsonl(
            &session_jsonl_path(sessions_root.path(), cwd.path(), second),
            "Two",
            Some(first),
        );
        let changes = p.poll_changes(now + Duration::from_secs(4)).unwrap();
        assert!(
            changes.iter().any(|c| matches!(c, ProviderChange::Rescan)),
            "expected Rescan for a new jsonl, got {changes:?}"
        );
    }

    #[test]
    fn omp_watch_next_deadline_matches_existing_debounce() {
        let mut p = make_provider();
        let now = Instant::now();
        p.last_poll = now;
        p.debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
        assert_eq!(
            p.next_deadline(),
            Some((now + TITLE_WATCH_DEBOUNCE).min(now + TITLE_FALLBACK_POLL))
        );
    }

    #[test]
    fn omp_watch_change_flows_through_supervisor_summary() {
        let sessions_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let id = "019fa182-eeee-7000-a63a-2352a49dbca2";
        let path = session_jsonl_path(sessions_root.path(), cwd.path(), id);
        write_official_jsonl(&path, "Before", None);

        let mut provider = make_provider();
        provider.session_dir_override = Some(sessions_root.path().to_path_buf());

        let mut registry = ProviderRegistry::empty_for_test(ProviderId::OMP);
        registry.register(Box::new(provider)).unwrap();
        let mut supervisor = SessionSupervisor::from_registry_for_test(registry);

        let listed = supervisor
            .select_provider_workspace(ProviderId::OMP, "ws-a", Some(cwd.path()))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key.session_id, id);
        assert_eq!(listed[0].title, "Before");

        write_session_title(&path, "After").unwrap();
        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"user","content":"x"}}}}"#
        )
        .unwrap();
        drop(f);
        let size_after = fs::metadata(&path).unwrap().len();
        let mtime_after = system_time_to_utc(fs::metadata(&path).unwrap().modified().unwrap());

        let now = Instant::now();
        let changes = supervisor
            .poll_provider_changes(now + Duration::from_secs(4))
            .unwrap();
        let summary = changes.iter().find_map(|c| match c {
            SessionSummaryChange::Upsert { summary, .. } => Some(summary),
            _ => None,
        });
        let summary = summary.expect("session is known after select; expected Upsert");
        assert_eq!(summary.key.session_id, id);
        assert_eq!(summary.title, "After");
        assert_eq!(summary.size, size_after);
        assert_eq!(summary.mtime, mtime_after);
    }

    #[test]
    fn omp_watch_delete_emits_removed_after_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let id = "019fa182-bbbb-7000-a63a-2352a49dbca2";
        let path = dir
            .path()
            .join(format!("2026-08-01T00-00-00-000Z_{id}.jsonl"));
        write_official_jsonl(&path, "Watched", None);

        let mut p = make_provider();
        let now = Instant::now();
        p.watched_cwd = Some(dir.path().to_path_buf());
        p.dirty_paths.insert(path.clone());
        p.debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
        p.last_poll = now;
        p.need_rescan = false;

        fs::remove_file(&path).unwrap();

        assert!(p.poll_changes(now).unwrap().is_empty());
        let changes = p.poll_changes(now + TITLE_WATCH_DEBOUNCE).unwrap();
        match changes.as_slice() {
            [ProviderChange::Removed(key)] => {
                assert_eq!(*key, SessionKey::omp(id));
            }
            other => panic!("expected one Removed, got {other:?}"),
        }
    }

    #[test]
    fn omp_spi_errors_when_session_path_is_none() {
        let mut p = make_provider();
        let session = ProviderSession {
            key: SessionKey::omp("s1"),
            title: "s1".into(),
            title_source: TitleSource::Fallback,
            parent_ref: None,
            path: None,
            cwd: PathBuf::from("/tmp"),
            modified_at: Utc::now(),
            size: 0,
        };
        let rename_err = p.rename_stored(&session, "x").unwrap_err().to_string();
        assert!(rename_err.contains("no path"), "{rename_err}");
        let delete_err = p.delete_stored(&session).unwrap_err().to_string();
        assert!(delete_err.contains("no path"), "{delete_err}");
        let load_err = p.load_transcript(&session).unwrap_err().to_string();
        assert!(load_err.contains("no path"), "{load_err}");
    }

    #[test]
    fn omp_watch_next_deadline_falls_back_to_poll_interval() {
        let mut p = make_provider();
        let now = Instant::now();
        p.debounce_until = None;
        p.last_poll = now;
        assert_eq!(p.next_deadline(), Some(now + TITLE_FALLBACK_POLL));
    }

    #[test]
    fn omp_available_errors_when_binary_missing() {
        let mut p = make_provider();
        p.omp_bin = "/nonexistent/omp-missing-bin".to_string();
        let err = p.available().unwrap_err().to_string();
        assert!(err.contains("omp not found"), "{err}");
        assert!(err.contains("/nonexistent/omp-missing-bin"), "{err}");
    }

    #[test]
    fn resolve_provisional_from_string_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        write_title_slot(&path, "");
        let mut f = File::options().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"user","content":"hello world"}}}}"#
        )
        .unwrap();

        let (title, kind) = resolve_display_title(&path, "abc");
        assert_eq!(
            (title.as_str(), kind),
            ("hello world", TitleKind::Provisional)
        );
    }
}
