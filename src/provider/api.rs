//! Provider-neutral SPI contracts and data models.
//!
//! This module defines the boundary between amux's session lifecycle
//! (PTY, live map, UI) and provider-specific logic (omp JSONL, spawn argv).
//! Providers implement [`AgentProvider`]; the shell and [`super::registry`]
//! only consume these types.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::provider::transcript::{DiffLine, ModifiedFile, TranscriptBlock};

/// Compile-time provider identifier.
///
/// strings are resolved through the registry, not stored per-session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId(&'static str);

impl ProviderId {
    pub const OMP: Self = Self("omp");

    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Strongly-typed session identity: `(provider, session_id)`.
///
/// Replaces bare `String` session IDs across the live map, focused session,
/// selection, unread, cache and lock so the compiler exposes missed callsites
/// when a second provider is added.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub provider: ProviderId,
    pub session_id: String,
}

impl SessionKey {
    pub fn new(provider: ProviderId, session_id: impl Into<String>) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
        }
    }

    /// Convenience for OMP keys (the only provider in this step).
    pub fn omp(session_id: impl Into<String>) -> Self {
        Self::new(ProviderId::OMP, session_id)
    }
}

/// Provenance of a session's display title.
///
/// Maps 1:1 to omp's [`crate::provider::omp::TitleKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSource {
    /// Non-empty `type:"title"` slot (LLM auto or `/rename`).
    Official,
    /// First user message while slot still empty.
    Provisional,
    /// Synthetic / id fallback.
    Fallback,
}

/// Provider-neutral session metadata.
///
/// Converted from `OmpDiskSession` before leaving `OmpProvider`; identity is
/// always the [`SessionKey`], not the optional `path`.
#[derive(Debug, Clone)]
pub struct ProviderSession {
    pub key: SessionKey,
    pub title: String,
    pub title_source: TitleSource,
    /// omp `header.parentSession` — uuid (fork) or source file path (branch).
    pub parent_ref: Option<String>,
    /// Cached/display path to the session file (e.g. JSONL). Not SPI identity.
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub modified_at: DateTime<Utc>,
    pub size: u64,
}

/// What the Provider wants amux to spawn.
///
/// Deliberately uses `String` to match [`crate::pty::PtySession::spawn`]'s
/// existing UTF-8 contract; omp continues to use `to_string_lossy` for cwd.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Which UI operations the provider supports.
///
/// Hides unsupported actions in the UI rather than providing no-op stubs.
#[derive(Debug, Clone, Copy)]
pub struct ProviderCapabilities {
    pub rename: bool,
    pub delete: bool,
    pub transcript: bool,
    pub modified_files: bool,
    pub live_rebind: bool,
}

impl ProviderCapabilities {
    /// OMP supports everything.
    pub const OMP: Self = Self {
        rename: true,
        delete: true,
        transcript: true,
        modified_files: true,
        live_rebind: true,
    };
}

/// Action returned by a live rename.
#[derive(Debug, Clone)]
pub enum LiveRenameAction {
    /// Bytes to write into the live PTY (e.g. `Ctrl-U + /rename + title + CR`).
    WritePty(Vec<u8>),
    /// Provider persisted the rename directly; no PTY write needed.
    Persisted,
}

/// A change detected by the provider's own change source.
#[derive(Debug, Clone)]
pub enum ProviderChange {
    /// A known session's metadata was updated.
    Upsert(ProviderSession),
    /// A session was removed from disk.
    Removed(SessionKey),
    /// Full rescan needed (watcher overflow, create/delete, error).
    Rescan,
}

/// Object-safe incremental scanner for modified files.
///
/// OMP adapts the existing [`crate::provider::transcript::ModifiedFilesScan`];
/// providers that don't support modified-files return `Ok(None)`.
pub trait ModifiedFilesScanner {
    /// Parse new bytes appended since the last call; returns true if changed.
    fn advance(&mut self, session: &ProviderSession) -> Result<bool>;
    /// Bumped whenever [`files`](Self::files) or retained changes change.
    fn version(&self) -> u64;
    /// Aggregated files, most recently modified first.
    fn files(&self) -> &[ModifiedFile];
    /// Diff lines for the file at `index`.
    fn render_diff(&self, file_index: usize) -> Vec<DiffLine>;
}

/// Synchronous, object-safe provider SPI.
///
/// amux's event loop and file access are synchronous; no async runtime is
/// introduced for a future provider. Every method must be explicitly
/// implemented — no default no-ops. Unsupported UI actions are hidden via
/// [`ProviderCapabilities`]; direct calls return a clear error.
pub trait AgentProvider {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;
    fn capabilities(&self) -> ProviderCapabilities;

    fn available(&self) -> Result<()>;
    fn list_sessions(&mut self, cwd: &Path) -> Result<Vec<ProviderSession>>;
    fn spawn_new(&self, cwd: &Path) -> Result<SpawnSpec>;
    fn spawn_resume(&self, cwd: &Path, session_id: &str) -> Result<SpawnSpec>;

    fn check_external_occupant(&self, session_id: &str) -> Result<()>;
    fn parent_refers_to(&self, parent_ref: &str, session_id: &str) -> bool;
    fn session_busy(&mut self, session: &ProviderSession, live: bool, pty_active: bool) -> bool;
    fn forget_session(&mut self, key: &SessionKey);
    fn normalize_title(&self, draft: &str) -> Result<String>;

    fn rename_live(&mut self, session: &ProviderSession, title: &str) -> Result<LiveRenameAction>;
    fn rename_stored(&mut self, session: &ProviderSession, title: &str) -> Result<()>;
    fn delete_stored(&mut self, session: &ProviderSession) -> Result<()>;

    fn select_workspace(&mut self, cwd: Option<&Path>) -> Result<()>;
    fn poll_changes(&mut self, now: Instant) -> Result<Vec<ProviderChange>>;
    fn next_deadline(&self) -> Option<Instant>;

    fn load_transcript(&mut self, session: &ProviderSession) -> Result<Vec<TranscriptBlock>>;
    fn modified_files_scanner(
        &mut self,
        session: &ProviderSession,
    ) -> Result<Option<Box<dyn ModifiedFilesScanner>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_distinguishes_same_id_across_providers() {
        let omp_key = SessionKey::omp("x");
        let fake_id = ProviderId::new("fake");
        let fake_key = SessionKey::new(fake_id, "x");

        assert_ne!(omp_key, fake_key);

        let mut map = std::collections::HashMap::new();
        map.insert(omp_key.clone(), 1);
        map.insert(fake_key.clone(), 2);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&omp_key), Some(&1));
        assert_eq!(map.get(&fake_key), Some(&2));
    }

    #[test]
    fn provider_id_display_is_stable() {
        assert_eq!(ProviderId::OMP.as_str(), "omp");
    }

    #[test]
    fn spawn_spec_keeps_existing_utf8_contract() {
        // SpawnSpec uses String, matching PtySession::spawn's signature.
        let spec = SpawnSpec {
            program: "omp".to_string(),
            args: vec!["--cwd".to_string(), "/some/cwd".to_string()],
            env: vec![("PI_TUI_SYNC_OUTPUT".to_string(), "1".to_string())],
            cwd: PathBuf::from("/some/cwd"),
        };
        // The fields are all String/Vec<String>, not OsString.
        let _program: String = spec.program;
        let _first_arg: String = spec.args[0].clone();
        let _first_env_key: String = spec.env[0].0.clone();
    }
}
