//! Shared `#[cfg(test)]` provider doubles.
//!
//! One [`FakeProvider`] implements the full [`AgentProvider`] surface so adding
//! an SPI method is a single compile error. Registry, supervisor, and later
//! Codex tests should reuse this module instead of copying a stub.
//!
//! This is not a Codex implementation. Tests may use `ProviderId::new("codex")`
//! as a second-provider stand-in without registering a real Codex backend.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};

use crate::config::AmuxConfig;
use crate::provider::api::{
    AgentProvider, LiveRenameAction, ModifiedFilesScanner, ProviderCapabilities, ProviderChange,
    ProviderId, ProviderSession, SessionKey, SpawnSpec, TitleSource,
};
use crate::provider::registry::ProviderRegistry;
use crate::provider::transcript::TranscriptBlock;

/// Shared mutable state for [`FakeProvider`].
pub struct FakeState {
    pub calls: Vec<String>,
    pub sessions: Vec<ProviderSession>,
    pub queued_changes: Vec<ProviderChange>,
    pub capabilities: ProviderCapabilities,
    pub busy: bool,
    pub occupant_error: Option<String>,
    pub rename_live_action: Option<LiveRenameAction>,
    /// Last session object passed into a mutating SPI method.
    pub last_session: Option<ProviderSession>,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            sessions: Vec::new(),
            queued_changes: Vec::new(),
            capabilities: ProviderCapabilities {
                rename: true,
                delete: true,
                transcript: true,
                modified_files: true,
                live_rebind: false,
            },
            busy: false,
            occupant_error: None,
            rename_live_action: None,
            last_session: None,
        }
    }
}

/// Test double: `Arc<Mutex<…>>` call log, configurable sessions, queued
/// [`ProviderChange`], capabilities, busy, and occupant flags.
///
/// Title rules match omp-ish sanitize: first line, strip controls, trim,
/// reject empty. Unsupported capability methods return a clear error.
pub struct FakeProvider {
    id: ProviderId,
    state: Arc<Mutex<FakeState>>,
}

impl FakeProvider {
    pub fn new(id: ProviderId) -> (Self, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        (
            Self {
                id,
                state: state.clone(),
            },
            state,
        )
    }

    fn log(&self, msg: impl Into<String>) {
        self.state.lock().unwrap().calls.push(msg.into());
    }
}

impl AgentProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    fn display_name(&self) -> &'static str {
        self.id.as_str()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.state.lock().unwrap().capabilities
    }

    fn available(&self) -> Result<()> {
        self.log("available");
        Ok(())
    }

    fn list_sessions(&mut self, _cwd: &Path) -> Result<Vec<ProviderSession>> {
        self.log("list_sessions");
        Ok(self.state.lock().unwrap().sessions.clone())
    }

    fn spawn_new(&self, cwd: &Path) -> Result<SpawnSpec> {
        self.log("spawn_new");
        Ok(SpawnSpec {
            program: self.id.as_str().into(),
            args: vec![],
            env: vec![],
            cwd: cwd.to_path_buf(),
        })
    }

    fn spawn_resume(&self, cwd: &Path, _session_id: &str) -> Result<SpawnSpec> {
        self.log("spawn_resume");
        Ok(SpawnSpec {
            program: self.id.as_str().into(),
            args: vec![],
            env: vec![],
            cwd: cwd.to_path_buf(),
        })
    }

    fn check_external_occupant(&self, _session_id: &str) -> Result<()> {
        self.log("check_external_occupant");
        if let Some(msg) = &self.state.lock().unwrap().occupant_error {
            return Err(anyhow!("{msg}"));
        }
        Ok(())
    }

    fn parent_refers_to(&self, parent_ref: &str, session_id: &str) -> bool {
        self.log(format!("parent_refers_to:{parent_ref}:{session_id}"));
        parent_ref == session_id || parent_ref.contains(session_id)
    }

    fn session_busy(&mut self, _session: &ProviderSession, _live: bool, _pty_active: bool) -> bool {
        self.log("session_busy");
        self.state.lock().unwrap().busy
    }

    fn forget_session(&mut self, key: &SessionKey) {
        self.log(format!("forget_session:{}", key.session_id));
    }

    fn normalize_title(&self, draft: &str) -> Result<String> {
        self.log("normalize_title");
        let first_line = draft.lines().next().unwrap_or("");
        let stripped: String = first_line.chars().filter(|c| !c.is_control()).collect();
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            Err(anyhow!("Title cannot be empty"))
        } else {
            Ok(trimmed.to_string())
        }
    }

    fn rename_live(&mut self, _session: &ProviderSession, title: &str) -> Result<LiveRenameAction> {
        self.log(format!("rename_live:{title}"));
        if !self.capabilities().rename {
            bail!("rename not supported by {}", self.id.as_str());
        }
        if let Some(action) = self.state.lock().unwrap().rename_live_action.clone() {
            return Ok(action);
        }
        Ok(LiveRenameAction::WritePty(
            format!("\x15/rename {title}\r").into_bytes(),
        ))
    }

    fn rename_stored(&mut self, session: &ProviderSession, title: &str) -> Result<()> {
        self.log(format!("rename_stored:{title}"));
        self.state.lock().unwrap().last_session = Some(session.clone());
        if !self.capabilities().rename {
            bail!("rename not supported by {}", self.id.as_str());
        }
        Ok(())
    }

    fn delete_stored(&mut self, session: &ProviderSession) -> Result<()> {
        self.log(format!("delete_stored:{}", session.key.session_id));
        self.state.lock().unwrap().last_session = Some(session.clone());
        if !self.capabilities().delete {
            bail!("delete not supported by {}", self.id.as_str());
        }
        Ok(())
    }

    fn select_workspace(&mut self, _cwd: Option<&Path>) -> Result<()> {
        self.log("select_workspace");
        self.state.lock().unwrap().queued_changes.clear();
        Ok(())
    }

    fn poll_changes(&mut self, _now: Instant) -> Result<Vec<ProviderChange>> {
        self.log("poll_changes");
        Ok(std::mem::take(
            &mut self.state.lock().unwrap().queued_changes,
        ))
    }

    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    fn load_transcript(&mut self, session: &ProviderSession) -> Result<Vec<TranscriptBlock>> {
        self.log("load_transcript");
        self.state.lock().unwrap().last_session = Some(session.clone());
        if !self.capabilities().transcript {
            bail!("transcript not supported by {}", self.id.as_str());
        }
        Ok(vec![])
    }

    fn modified_files_scanner(
        &mut self,
        _session: &ProviderSession,
    ) -> Result<Option<Box<dyn ModifiedFilesScanner>>> {
        self.log("modified_files_scanner");
        if !self.capabilities().modified_files {
            return Ok(None);
        }
        Ok(None)
    }
}

/// Build a disk session owned by `provider` (path is `/tmp/<id>/<session>.jsonl`).
pub fn session_on(
    provider: ProviderId,
    id: &str,
    cwd: &str,
    title: &str,
    source: TitleSource,
    parent: Option<&str>,
    mtime: DateTime<Utc>,
    size: u64,
) -> ProviderSession {
    ProviderSession {
        key: SessionKey::new(provider, id),
        title: title.into(),
        title_source: source,
        parent_ref: parent.map(|s| s.into()),
        path: Some(PathBuf::from(format!(
            "/tmp/{}/{id}.jsonl",
            provider.as_str()
        ))),
        cwd: PathBuf::from(cwd),
        modified_at: mtime,
        size,
    }
}

/// Compile-time canary: `AgentProvider` stays object-safe for `Box<dyn …>`.
fn _assert_object_safe(_: &dyn AgentProvider) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_provider_is_object_safe() {
        let (fake, _) = FakeProvider::new(ProviderId::new("codex"));
        _assert_object_safe(&fake);
        let boxed: Box<dyn AgentProvider> = Box::new(fake);
        assert_eq!(boxed.id().as_str(), "codex");
    }

    #[test]
    fn fake_provider_exercises_every_spi_method() {
        let id = ProviderId::new("codex");
        let (mut p, state) = FakeProvider::new(id);
        let cwd = Path::new("/ws");
        let t = Utc::now();
        let sess = session_on(id, "s1", "/ws", "title", TitleSource::Fallback, None, t, 1);
        state.lock().unwrap().sessions = vec![sess.clone()];

        assert_eq!(p.id(), id);
        assert_eq!(p.display_name(), "codex");
        let caps = p.capabilities();
        assert!(caps.rename && caps.delete && caps.transcript && caps.modified_files);
        assert!(!caps.live_rebind);
        p.available().unwrap();
        assert_eq!(p.list_sessions(cwd).unwrap().len(), 1);
        assert_eq!(p.spawn_new(cwd).unwrap().program, "codex");
        assert_eq!(p.spawn_resume(cwd, "s1").unwrap().program, "codex");
        p.check_external_occupant("s1").unwrap();
        assert!(p.parent_refers_to("s1", "s1"));
        assert!(!p.session_busy(&sess, false, false));
        p.forget_session(&sess.key);
        assert_eq!(p.normalize_title("  ok\nmore").unwrap(), "ok");
        match p.rename_live(&sess, "t").unwrap() {
            LiveRenameAction::WritePty(bytes) => {
                assert_eq!(bytes, b"\x15/rename t\r");
            }
            other => panic!("expected WritePty, got {other:?}"),
        }
        p.rename_stored(&sess, "t").unwrap();
        p.delete_stored(&sess).unwrap();
        p.select_workspace(Some(cwd)).unwrap();
        assert!(p.poll_changes(Instant::now()).unwrap().is_empty());
        assert!(p.next_deadline().is_none());
        assert!(p.load_transcript(&sess).unwrap().is_empty());
        assert!(p.modified_files_scanner(&sess).unwrap().is_none());

        let calls = state.lock().unwrap().calls.clone();
        for required in [
            "available",
            "list_sessions",
            "spawn_new",
            "spawn_resume",
            "check_external_occupant",
            "parent_refers_to:s1:s1",
            "session_busy",
            "forget_session:s1",
            "normalize_title",
            "rename_live:t",
            "rename_stored:t",
            "delete_stored:s1",
            "select_workspace",
            "poll_changes",
            "load_transcript",
            "modified_files_scanner",
        ] {
            assert!(
                calls.iter().any(|c| c == required),
                "missing SPI call {required}: {calls:?}"
            );
        }
    }

    #[test]
    fn fake_provider_errors_when_capability_disabled() {
        let id = ProviderId::new("codex");
        let (mut p, state) = FakeProvider::new(id);
        {
            let mut st = state.lock().unwrap();
            st.capabilities.rename = false;
            st.capabilities.delete = false;
            st.capabilities.transcript = false;
            st.capabilities.modified_files = false;
        }
        let sess = session_on(
            id,
            "s1",
            "/ws",
            "title",
            TitleSource::Fallback,
            None,
            Utc::now(),
            1,
        );
        let rename_live = p.rename_live(&sess, "t").unwrap_err().to_string();
        assert!(rename_live.contains("codex"), "{rename_live}");
        let rename_stored = p.rename_stored(&sess, "t").unwrap_err().to_string();
        assert!(rename_stored.contains("codex"), "{rename_stored}");
        let delete = p.delete_stored(&sess).unwrap_err().to_string();
        assert!(delete.contains("codex"), "{delete}");
        let transcript = p.load_transcript(&sess).unwrap_err().to_string();
        assert!(transcript.contains("codex"), "{transcript}");
        assert!(p.modified_files_scanner(&sess).unwrap().is_none());
    }

    #[test]
    fn registry_holds_omp_and_second_provider_without_changing_default() {
        let mut reg = ProviderRegistry::from_config(&AmuxConfig::default()).unwrap();
        assert_eq!(reg.default_id(), ProviderId::OMP);
        let (second, _) = FakeProvider::new(ProviderId::new("codex"));
        reg.register(Box::new(second)).unwrap();

        let mut ids: Vec<_> = reg.ids().into_iter().map(|id| id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["codex", "omp"]);
        assert_eq!(reg.default_id(), ProviderId::OMP);
        assert_eq!(reg.get(ProviderId::OMP).unwrap().id(), ProviderId::OMP);
        assert_eq!(
            reg.get(ProviderId::new("codex")).unwrap().id().as_str(),
            "codex"
        );
    }
}
