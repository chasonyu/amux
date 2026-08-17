//! SessionSupervisor: disk discovery + live PtySession map.
//!
//! Provider-neutral: all provider logic is routed through [`ProviderRegistry`]
//! → [`crate::provider::api::AgentProvider`]. The supervisor owns the live PTY
//! map, lock lifecycle, and reconcile algorithms (synthetic adopt, fork/branch
//! rebind, title merge, busy→idle). It never imports OMP-specific types.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::appearance::HostSurface;
use crate::config::AmuxConfig;
use crate::lock::SessionLock;
use crate::provider::api::{
    AgentProvider, LiveRenameAction, ModifiedFilesScanner, ProviderChange, ProviderId,
    ProviderSession, SessionKey, SpawnSpec, TitleSource,
};
use crate::provider::registry::ProviderRegistry;
use crate::provider::transcript::TranscriptBlock;
use crate::pty::PtySession;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Disk,
    Starting,
    Running,
    Exited,
    Error,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Error => "error",
        }
    }
}

/// Incremental session-list update emitted by [`SessionSupervisor::poll_provider_changes`].
#[derive(Debug, Clone)]
pub enum SessionSummaryChange {
    /// A known session's metadata was refreshed (title, busy, mtime, size).
    Upsert {
        summary: SessionSummary,
        became_idle: bool,
    },
    /// Full rescan: the Shell replaces its entire session_list.
    ReplaceAll {
        sessions: Vec<SessionSummary>,
        rebinds: Vec<(SessionKey, SessionKey)>,
        became_idle: Vec<SessionKey>,
    },
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub key: SessionKey,
    pub workspace_id: String,
    pub title: String,
    pub title_source: TitleSource,
    /// True when provider header has `parent_ref` (fork / file branch).
    pub is_fork: bool,
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub mtime: DateTime<Utc>,
    pub size: u64,
    pub live: bool,
    pub status: SessionStatus,
    /// Live PTY whose main turn is active or whose background jobs are pending.
    pub agent_busy: bool,
    /// Turn finished while this row was not being viewed — clear on select/attach.
    pub unread: bool,
}

struct LiveEntry {
    key: SessionKey,
    pty: PtySession,
    /// Released once the child exits; held while alive to prevent concurrent attach.
    lock: Option<SessionLock>,
    status: SessionStatus,
    title: String,
    title_source: TitleSource,
    cwd: PathBuf,
    workspace_id: String,
    /// When the session was spawned — used to match "new-N" entries to
    /// omp's on-disk uuid after omp writes the session file.
    spawned_at: DateTime<Utc>,
}

/// Arguments [`PtySession::spawn`] would receive, derived from a [`SpawnSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySpawnParams<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    pub rows: u16,
    pub cols: u16,
    pub kitty: bool,
    pub surface: HostSurface,
}

pub struct SessionSupervisor {
    registry: ProviderRegistry,
    live: HashMap<SessionKey, LiveEntry>,
    /// Synthetic ids for brand-new sessions not yet on disk.
    next_new: u64,
    /// Detached kill threads from close_session — joined in shutdown to
    /// prevent orphaning when amux exits mid-ladder. (§4.2.8 / E14)
    kill_threads: Vec<JoinHandle<()>>,
    /// Whether the outer terminal supports kitty keyboard protocol.
    kitty_keyboard: bool,
    /// Host FG/BG mirrored into new/live PTY palettes (OSC 10/11 + paint).
    host_surface: HostSurface,
    /// Last-seen disk session ids per workspace — used to detect newly created
    /// fork/branch children without mistaking historical siblings for rebinds.
    known_disk_ids: HashMap<String, HashSet<String>>,
    /// `(old_key, new_key)` from the latest full reconcile pass.
    pending_rebinds: Vec<(SessionKey, SessionKey)>,
    /// Previous busy state per session — for became_idle detection.
    prev_busy: HashMap<SessionKey, bool>,
    /// Active change-source watch: (provider, workspace_id, cwd).
    current_watch: Option<(ProviderId, String, PathBuf)>,
    /// Last-listed / last-upserted disk sessions (path/cwd/title/parent/mtime/size).
    known_sessions: HashMap<SessionKey, ProviderSession>,
}

impl SessionSupervisor {
    pub fn new(config: AmuxConfig, kitty_keyboard: bool) -> Self {
        let registry =
            ProviderRegistry::from_config(&config).expect("omp provider registration must succeed");
        Self::from_parts(registry, kitty_keyboard)
    }

    /// Same as [`Self::new`] but takes a ready registry (no `from_config`).
    pub fn from_registry_for_test(registry: ProviderRegistry) -> Self {
        Self::from_parts(registry, false)
    }

    #[cfg(test)]
    fn known_disk_ids_contains(&self, workspace_id: &str) -> bool {
        self.known_disk_ids.contains_key(workspace_id)
    }

    fn from_parts(registry: ProviderRegistry, kitty_keyboard: bool) -> Self {
        Self {
            registry,
            live: HashMap::new(),
            next_new: 1,
            kill_threads: Vec::new(),
            kitty_keyboard,
            host_surface: HostSurface::fallback(crate::appearance::Appearance::Dark),
            known_disk_ids: HashMap::new(),
            pending_rebinds: Vec::new(),
            prev_busy: HashMap::new(),
            current_watch: None,
            known_sessions: HashMap::new(),
        }
    }

    // --- provider access helper ---

    fn default_provider(&mut self) -> Result<&mut (dyn AgentProvider + '_)> {
        let id = self.registry.default_id();
        self.registry.get_mut(id)
    }

    /// Registry default — Shell must not hardcode `ProviderId::OMP`.
    pub fn default_provider_id(&self) -> ProviderId {
        self.registry.default_id()
    }

    // --- host surface ---

    /// Sync host surface into all live PTYs (palette + Mode 2031 notify for omp).
    pub fn set_host_surface(&mut self, surface: HostSurface) {
        self.host_surface = surface;
        for entry in self.live.values_mut() {
            entry.pty.set_host_surface(surface);
        }
    }

    // --- rebinds ---

    /// Drain provider in-process session rebinds (fork / file-creating branch).
    pub fn drain_rebinds(&mut self) -> Vec<(SessionKey, SessionKey)> {
        std::mem::take(&mut self.pending_rebinds)
    }

    // --- workspace selection (replaces list_for_workspace) ---

    /// Set up the provider's change source for `cwd` and return the initial
    /// session list. Pass `cwd=None` to clear monitoring and return empty.
    ///
    /// Called on startup, workspace add/remove/move, or selected workspace change.
    pub fn select_provider_workspace(
        &mut self,
        provider: ProviderId,
        workspace_id: &str,
        cwd: Option<&Path>,
    ) -> Result<Vec<SessionSummary>> {
        self.pending_rebinds.clear();

        // Change-source switch: provider discards the old workspace queue.
        {
            let p = self.registry.get_mut(provider)?;
            p.select_workspace(cwd)?;
        }

        // cwd=None → clear monitoring for this provider/workspace and return empty.
        let Some(cwd) = cwd else {
            if self
                .current_watch
                .as_ref()
                .is_some_and(|(p, w, _)| *p == provider && w == workspace_id)
            {
                self.current_watch = None;
            }
            self.known_sessions.retain(|k, _| k.provider != provider);
            self.known_disk_ids.remove(workspace_id);
            return Ok(Vec::new());
        };

        if let Some((prev_p, _, prev_cwd)) = self.current_watch.clone() {
            if prev_p == provider && prev_cwd != cwd {
                self.known_sessions
                    .retain(|k, s| !(k.provider == provider && s.cwd == prev_cwd));
            }
        }
        self.current_watch = Some((provider, workspace_id.to_string(), cwd.to_path_buf()));

        let sessions = {
            let p = self.registry.get_mut(provider)?;
            p.list_sessions(cwd)?
        };

        for s in &sessions {
            self.known_sessions.insert(s.key.clone(), s.clone());
        }

        let summaries = self.reconcile_sessions(provider, workspace_id, cwd, &sessions)?;

        // Drop prev_busy only for this provider's sessions that left the new list
        // so remaining live keys on other providers can still become idle.
        let keep: HashSet<SessionKey> = summaries.iter().map(|s| s.key.clone()).collect();
        self.prev_busy
            .retain(|k, _| k.provider != provider || keep.contains(k));

        Ok(summaries)
    }

    /// Full reconcile: synthetic adopt, fork/branch rebind, title merge, busy.
    /// Returns the summary list and populates `pending_rebinds`.
    fn reconcile_sessions(
        &mut self,
        provider: ProviderId,
        workspace_id: &str,
        cwd: &Path,
        disk: &[ProviderSession],
    ) -> Result<Vec<SessionSummary>> {
        // Reconcile "new-N" live sessions: if omp has written a session file
        // with its own uuid, adopt that uuid as the live key so the sidebar
        // shows one entry, not two, and occupied-detection works. (§5.2)
        // Synthetic adopt is independent of the live_rebind capability gate.
        let mut synthetics: Vec<(SessionKey, DateTime<Utc>)> = self
            .live
            .iter()
            .filter(|(k, e)| {
                k.provider == provider
                    && k.session_id.starts_with("new-")
                    && e.workspace_id == workspace_id
            })
            .map(|(k, e)| (k.clone(), e.spawned_at))
            .collect();
        // Oldest spawn first so concurrent news pair stably with disk mtimes.
        synthetics.sort_by_key(|(_, spawned_at)| *spawned_at);
        let mut claimed_disk: HashSet<String> = HashSet::new();
        for (syn_key, spawned_at) in synthetics {
            let matched_id = pick_synthetic_disk_match_for_key(disk, &syn_key, spawned_at, |id| {
                self.live.contains_key(&SessionKey::new(provider, id)) || claimed_disk.contains(id)
            })
            .map(|d| d.key.session_id.clone());
            let Some(uuid) = matched_id else {
                continue;
            };
            claimed_disk.insert(uuid.clone());
            let Some(d) = disk.iter().find(|d| d.key.session_id == uuid) else {
                continue;
            };
            let new_key = SessionKey::new(provider, &uuid);
            if let Some(mut entry) = self.live.remove(&syn_key) {
                // forget old synthetic
                {
                    let p = self.registry.get_mut(provider)?;
                    p.forget_session(&syn_key);
                }
                // Release the old "new-N" lock and acquire under uuid.
                drop(entry.lock.take());
                let lock = SessionLock::try_acquire(&new_key).ok();
                entry.lock = lock;
                entry.key = new_key.clone();
                apply_disk_title_to_live(&mut entry.title, &mut entry.title_source, d);
                self.live.insert(new_key.clone(), entry);
                self.pending_rebinds.push((syn_key, new_key));
            }
        }

        // omp /fork and file-creating /branch rebind the same PTY to a new
        // JSONL; migrate our live map + notify the UI via pending_rebinds.
        // Only when the provider advertises live_rebind — never call
        // parent_refers_to when the capability is false.
        let caps = {
            let p = self.registry.get_mut(provider)?;
            p.capabilities()
        };
        if caps.live_rebind {
            self.reconcile_fork_rebinds(provider, workspace_id, disk);
        }

        // Keep live titles in sync with disk (LLM /rename / firstMessage).
        // Never let a half-written jsonl Fallback(id) clobber "New session".
        for d in disk {
            let key = &d.key;
            if key.provider != provider {
                continue;
            }
            if let Some(entry) = self.live.get_mut(key) {
                apply_disk_title_to_live(&mut entry.title, &mut entry.title_source, d);
            }
        }

        let mut out = Vec::with_capacity(disk.len());
        for d in disk {
            if d.key.provider != provider {
                continue;
            }
            out.push(
                self.build_summary(provider, workspace_id, cwd, d.clone())?
                    .0,
            );
        }

        // Include live "new" sessions not yet on disk list
        for (key, entry) in &self.live {
            if key.provider != provider {
                continue;
            }
            if entry.workspace_id == workspace_id && !out.iter().any(|s| s.key == *key) {
                out.insert(
                    0,
                    SessionSummary {
                        key: key.clone(),
                        workspace_id: workspace_id.to_string(),
                        title: entry.title.clone(),
                        title_source: entry.title_source,
                        is_fork: false,
                        path: None,
                        cwd: entry.cwd.clone(),
                        mtime: Utc::now(),
                        size: 0,
                        live: true,
                        status: entry.status,
                        agent_busy: false,
                        unread: false,
                    },
                );
            }
        }

        self.refresh_known_sessions(provider, workspace_id, cwd, disk);
        Ok(out)
    }

    fn refresh_known_sessions(
        &mut self,
        provider: ProviderId,
        workspace_id: &str,
        cwd: &Path,
        disk: &[ProviderSession],
    ) {
        self.known_sessions
            .retain(|k, s| k.provider != provider || s.cwd != cwd);
        for d in disk {
            if d.key.provider == provider {
                self.known_sessions.insert(d.key.clone(), d.clone());
            }
        }
        let synthetics: Vec<(SessionKey, ProviderSession)> = self
            .live
            .iter()
            .filter(|(key, entry)| {
                key.provider == provider
                    && entry.workspace_id == workspace_id
                    && !self.known_sessions.contains_key(*key)
            })
            .map(|(key, entry)| {
                (
                    key.clone(),
                    ProviderSession {
                        key: key.clone(),
                        title: entry.title.clone(),
                        title_source: entry.title_source,
                        parent_ref: None,
                        path: None,
                        cwd: entry.cwd.clone(),
                        modified_at: entry.spawned_at,
                        size: 0,
                    },
                )
            })
            .collect();
        for (key, session) in synthetics {
            self.known_sessions.insert(key, session);
        }
    }

    fn build_summary(
        &mut self,
        provider: ProviderId,
        workspace_id: &str,
        cwd: &Path,
        d: ProviderSession,
    ) -> Result<(SessionSummary, bool)> {
        let live_entry = self.live.get(&d.key);
        let live = live_entry.is_some();
        let status = live_entry.map(|e| e.status).unwrap_or(SessionStatus::Disk);
        // Live title is authoritative after merge policy (keeps "New session"
        // while disk still only has Fallback id).
        let (title, title_source) = match live_entry {
            Some(e) => (e.title.clone(), e.title_source),
            None => (d.title.clone(), d.title_source),
        };
        let path = d.path.clone();
        let agent_busy = {
            let p = self.registry.get_mut(provider)?;
            p.session_busy(
                &d,
                live,
                matches!(status, SessionStatus::Starting | SessionStatus::Running),
            )
        };
        // became_idle must read the *previous* busy flag, then record the new one.
        let prev = self.prev_busy.get(&d.key).copied().unwrap_or(false);
        let became_idle = prev && !agent_busy;
        self.prev_busy.insert(d.key.clone(), agent_busy);

        Ok((
            SessionSummary {
                key: d.key,
                workspace_id: workspace_id.to_string(),
                title,
                title_source,
                is_fork: d.parent_ref.is_some(),
                path,
                cwd: cwd.to_path_buf(),
                mtime: d.modified_at,
                size: d.size,
                live,
                status,
                agent_busy,
                unread: false,
            },
            became_idle,
        ))
    }

    // --- incremental change polling ---

    /// Poll the provider's change source and convert to [`SessionSummaryChange`].
    ///
    /// Known MODIFY → [`SessionSummaryChange::Upsert`] (single session refresh).
    /// Unknown Upsert / Removed / Rescan → [`SessionSummaryChange::ReplaceAll`]
    /// (full reconcile including synthetic adopt, fork rebind, title merge).
    pub fn poll_provider_changes(&mut self, now: Instant) -> Result<Vec<SessionSummaryChange>> {
        let Some((provider_id, workspace_id, cwd)) = self.current_watch.clone() else {
            return Ok(Vec::new());
        };

        let changes = {
            let p = self.registry.get_mut(provider_id)?;
            p.poll_changes(now)?
        };

        let mut out = Vec::new();
        for change in changes {
            match change {
                ProviderChange::Upsert(session) => {
                    // Drop late events from a stale workspace or other provider.
                    if session.cwd != cwd || session.key.provider != provider_id {
                        continue;
                    }
                    let known = self.known_sessions.contains_key(&session.key)
                        || self.live.contains_key(&session.key);
                    if known {
                        if let Some(entry) = self.live.get_mut(&session.key) {
                            apply_disk_title_to_live(
                                &mut entry.title,
                                &mut entry.title_source,
                                &session,
                            );
                        }
                        self.known_sessions
                            .insert(session.key.clone(), session.clone());
                        let (summary, became_idle) =
                            self.build_summary(provider_id, &workspace_id, &cwd, session)?;
                        out.push(SessionSummaryChange::Upsert {
                            summary,
                            became_idle,
                        });
                    } else {
                        out.extend(self.full_rescan()?);
                    }
                }
                ProviderChange::Removed(_) | ProviderChange::Rescan => {
                    out.extend(self.full_rescan()?);
                }
            }
        }

        Ok(out)
    }

    /// Next deadline from the provider (debounce / fallback poll).
    pub fn next_provider_deadline(&self) -> Option<Instant> {
        let id = self
            .current_watch
            .as_ref()
            .map(|(p, _, _)| *p)
            .unwrap_or_else(|| self.registry.default_id());
        self.registry.get(id).ok().and_then(|p| p.next_deadline())
    }

    fn full_rescan(&mut self) -> Result<Vec<SessionSummaryChange>> {
        let Some((provider, workspace_id, cwd)) = self.current_watch.clone() else {
            return Ok(Vec::new());
        };

        // Snapshot previous busy *before* reconcile writes the new values.
        let prev_snapshot = self.prev_busy.clone();

        let sessions = {
            let p = self.registry.get_mut(provider)?;
            p.list_sessions(&cwd)?
        };
        let summaries = self.reconcile_sessions(provider, &workspace_id, &cwd, &sessions)?;
        let rebinds = std::mem::take(&mut self.pending_rebinds);

        let mut became_idle = Vec::new();
        for s in &summaries {
            let prev = prev_snapshot.get(&s.key).copied().unwrap_or(false);
            if prev && !s.agent_busy {
                became_idle.push(s.key.clone());
            }
        }

        Ok(vec![SessionSummaryChange::ReplaceAll {
            sessions: summaries,
            rebinds,
            became_idle,
        }])
    }

    // --- live title ---

    /// Optimistic sidebar title after Nav rename / `/rename` inject.
    pub fn set_live_title(&mut self, key: &SessionKey, title: String) {
        if let Some(entry) = self.live.get_mut(key) {
            entry.title = title;
            entry.title_source = TitleSource::Official;
        }
    }

    // --- fork/branch rebind ---

    fn reconcile_fork_rebinds(
        &mut self,
        provider: ProviderId,
        workspace_id: &str,
        disk: &[ProviderSession],
    ) {
        let current_ids: HashSet<String> = disk
            .iter()
            .filter(|d| d.key.provider == provider)
            .map(|d| d.key.session_id.clone())
            .collect();

        // First scan for this workspace: seed only (avoid adopting old forks).
        {
            let prev = self
                .known_disk_ids
                .entry(workspace_id.to_string())
                .or_default();
            if prev.is_empty() {
                *prev = current_ids;
                return;
            }
        }

        let new_ids: HashSet<String> = {
            let prev = self
                .known_disk_ids
                .get(workspace_id)
                .cloned()
                .unwrap_or_default();
            current_ids.difference(&prev).cloned().collect()
        };

        let mut newcomers: Vec<&ProviderSession> = disk
            .iter()
            .filter(|d| d.key.provider == provider && new_ids.contains(&d.key.session_id))
            .collect();
        // Newest first so a burst of creates still prefers the latest child.
        newcomers.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));

        let mut migrated_parents: HashSet<String> = HashSet::new();
        for child in newcomers {
            let Some(parent) = child.parent_ref.as_deref() else {
                continue;
            };
            if self.live.contains_key(&child.key) {
                continue;
            }
            let parent_live = self
                .live
                .iter()
                .find(|(live_key, entry)| {
                    live_key.provider == provider
                        && entry.workspace_id == workspace_id
                        && !live_key.session_id.starts_with("new-")
                        && entry.status != SessionStatus::Exited
                        && {
                            let p = self.registry.get(provider).ok();
                            p.is_some_and(|p| p.parent_refers_to(parent, &live_key.session_id))
                        }
                })
                .map(|(k, _)| k.clone());
            let Some(old_key) = parent_live else {
                continue;
            };
            if migrated_parents.contains(&old_key.session_id) {
                continue;
            }
            if self.rebind_live(provider, &old_key, child) {
                migrated_parents.insert(old_key.session_id);
            }
        }

        self.known_disk_ids
            .insert(workspace_id.to_string(), current_ids);
    }

    fn rebind_live(
        &mut self,
        provider: ProviderId,
        old_key: &SessionKey,
        child: &ProviderSession,
    ) -> bool {
        let Some(mut entry) = self.live.remove(old_key) else {
            return false;
        };
        {
            let p = self.registry.get_mut(provider).ok();
            if let Some(p) = p {
                p.forget_session(old_key);
            }
        }
        drop(entry.lock.take());
        let new_key = SessionKey::new(provider, &child.key.session_id);
        entry.lock = SessionLock::try_acquire(&new_key).ok();
        entry.key = new_key.clone();
        apply_disk_title_to_live(&mut entry.title, &mut entry.title_source, child);
        self.live.insert(new_key.clone(), entry);
        self.pending_rebinds.push((old_key.clone(), new_key));
        tracing::info!(
            target: "amux",
            old = %old_key.session_id,
            new = %child.key.session_id,
            "omp session rebind (fork/branch)"
        );
        true
    }

    // --- PTY access ---

    pub fn get_mut(&mut self, key: &SessionKey) -> Option<&mut PtySession> {
        self.live.get_mut(key).map(|e| &mut e.pty)
    }

    pub fn get(&self, key: &SessionKey) -> Option<&PtySession> {
        self.live.get(key).map(|e| &e.pty)
    }

    pub fn is_live(&self, key: &SessionKey) -> bool {
        self.live.contains_key(key)
    }

    // --- spawn / attach ---

    /// Lazy spawn / attach resume. Returns session key.
    ///
    /// Order: external occupant check → flock → available → spawn spec → PTY spawn.
    pub fn attach_resume(
        &mut self,
        workspace_id: &str,
        cwd: &Path,
        key: &SessionKey,
        title: &str,
        title_source: TitleSource,
        rows: u16,
        cols: u16,
    ) -> Result<SessionKey> {
        // If the session is live but exited, remove the dead entry and
        // respawn via `omp --resume`. (§4.2.2 / §11.5)
        if let Some(entry) = self.live.get(key) {
            if entry.status == SessionStatus::Exited {
                self.live.remove(key);
            } else {
                return Ok(key.clone());
            }
        }

        let provider_id = key.provider;

        let (lock, spec) = run_resume_attach_guards(
            self,
            |this| {
                let p = this.registry.get_mut(provider_id)?;
                p.check_external_occupant(&key.session_id)
            },
            |_this| SessionLock::try_acquire(key),
            |this| {
                let p = this.registry.get_mut(provider_id)?;
                p.available()
            },
            |this| {
                let p = this.registry.get_mut(provider_id)?;
                p.spawn_resume(cwd, &key.session_id)
            },
        )?;
        let params =
            pty_spawn_params_from_spec(&spec, rows, cols, self.kitty_keyboard, self.host_surface);
        let pty = PtySession::spawn(
            params.program,
            params.args,
            params.cwd,
            params.rows,
            params.cols,
            params.env,
            params.kitty,
            params.surface,
        )
        .with_context(|| format!("spawn {} --resume {}", spec.program, key.session_id))?;

        self.live.insert(
            key.clone(),
            LiveEntry {
                key: key.clone(),
                pty,
                lock: Some(lock),
                status: SessionStatus::Starting,
                title: title.to_string(),
                title_source,
                cwd: cwd.to_path_buf(),
                workspace_id: workspace_id.to_string(),
                spawned_at: Utc::now(),
            },
        );
        Ok(key.clone())
    }

    /// Spawn a brand-new omp session.
    ///
    /// Order: available → synthetic ID → flock → spawn spec → PTY spawn.
    pub fn attach_new(
        &mut self,
        workspace_id: &str,
        cwd: &Path,
        rows: u16,
        cols: u16,
    ) -> Result<SessionKey> {
        let provider_id = self.registry.default_id();

        // 1. available
        {
            let p = self.registry.get_mut(provider_id)?;
            p.available()?;
        }
        // 2. synthetic ID
        let session_id = format!("new-{}", self.next_new);
        self.next_new += 1;
        let key = SessionKey::new(provider_id, &session_id);
        // 3. flock
        let lock = SessionLock::try_acquire(&key)?;
        // 4. spawn spec
        let spec = {
            let p = self.registry.get_mut(provider_id)?;
            p.spawn_new(cwd)?
        };
        let params =
            pty_spawn_params_from_spec(&spec, rows, cols, self.kitty_keyboard, self.host_surface);
        let pty = PtySession::spawn(
            params.program,
            params.args,
            params.cwd,
            params.rows,
            params.cols,
            params.env,
            params.kitty,
            params.surface,
        )
        .context("spawn omp")?;

        self.live.insert(
            key.clone(),
            LiveEntry {
                key: key.clone(),
                pty,
                lock: Some(lock),
                status: SessionStatus::Starting,
                title: "New session".into(),
                title_source: TitleSource::Fallback,
                cwd: cwd.to_path_buf(),
                workspace_id: workspace_id.to_string(),
                spawned_at: Utc::now(),
            },
        );
        Ok(key)
    }

    // --- close / kill ---

    pub fn close_session(&mut self, key: &SessionKey) {
        {
            let p = self.registry.get_mut(key.provider).ok();
            if let Some(p) = p {
                p.forget_session(key);
            }
        }
        self.prev_busy.remove(key);
        if let Some(mut entry) = self.live.remove(key) {
            // Hold the flock until the kill ladder finishes so a
            // close→reattach race can't attach a new resume while
            // the old child is still dying. (§2 occupied / §4.2.11.5)
            let handle = std::thread::spawn(move || {
                entry.pty.kill_process_group();
                drop(entry.lock.take());
            });
            self.kill_threads.push(handle);
        }
    }

    /// Kill a live session and wait for the kill ladder before returning.
    /// Use before deleting its jsonl so a dying omp cannot recreate the file.
    pub fn close_session_blocking(&mut self, key: &SessionKey) {
        {
            let p = self.registry.get_mut(key.provider).ok();
            if let Some(p) = p {
                p.forget_session(key);
            }
        }
        self.prev_busy.remove(key);
        if let Some(mut entry) = self.live.remove(key) {
            entry.pty.kill_process_group();
            drop(entry.lock.take());
        }
    }

    /// Join any detached kill threads from [`Self::close_session`].
    pub fn join_pending_kills(&mut self) {
        for handle in std::mem::take(&mut self.kill_threads) {
            let _ = handle.join();
        }
    }

    /// Live session keys belonging to `workspace_id` (non-exited).
    pub fn live_ids_for_workspace(&self, workspace_id: &str) -> Vec<SessionKey> {
        self.live
            .iter()
            .filter(|(_, e)| e.workspace_id == workspace_id && !e.pty.is_exited())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Close every live session for a workspace. Returns how many were closed.
    pub fn close_workspace_sessions(&mut self, workspace_id: &str) -> usize {
        let keys = self.live_ids_for_workspace(workspace_id);
        let n = keys.len();
        for key in keys {
            self.close_session(&key);
        }
        self.known_disk_ids.remove(workspace_id);
        n
    }

    /// Drain host-bound escapes (OSC 52 etc.) for every live session.
    ///
    /// Only bytes from the `focused` session reach the outer terminal;
    /// bytes from background sessions are discarded. (§4.2.11.2)
    pub fn drain_host_outbound(&mut self, focused: Option<&SessionKey>) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, entry) in &mut self.live {
            let host = entry.pty.take_host_outbound();
            if host.is_empty() {
                continue;
            }
            if Some(key) == focused {
                out.extend_from_slice(&host);
            }
        }
        out
    }

    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        for entry in self.live.values_mut() {
            let _ = entry.pty.resize(rows, cols);
        }
    }

    /// Poll child liveness. Returns keys that just transitioned to exited/error.
    pub fn poll_exits(&mut self) -> Vec<SessionKey> {
        let mut just_exited = Vec::new();
        let keys: Vec<SessionKey> = self.live.keys().cloned().collect();
        for key in keys {
            let Some(entry) = self.live.get_mut(&key) else {
                continue;
            };
            if entry.status == SessionStatus::Starting && entry.pty.is_ready() {
                entry.status = SessionStatus::Running;
            }
            if entry.pty.is_exited() && entry.lock.is_some() {
                let success = entry.pty.exit_success();
                entry.status = match success {
                    Some(true) => SessionStatus::Exited,
                    _ => SessionStatus::Error,
                };
                entry.lock = None;
                just_exited.push(key);
            }
        }
        for key in &just_exited {
            let p = self.registry.get_mut(key.provider).ok();
            if let Some(p) = p {
                p.forget_session(key);
            }
        }
        just_exited
    }

    // --- provider-routed operations ---

    /// Normalize a title draft through the provider's rules.
    pub fn normalize_title(&mut self, draft: &str) -> Result<String> {
        let p = self.default_provider()?;
        p.normalize_title(draft)
    }

    /// Live rename: returns the bytes to write into the PTY.
    pub fn rename_live(&mut self, key: &SessionKey, title: &str) -> Result<LiveRenameAction> {
        let session = self.session_for_key(key);
        let p = self.registry.get_mut(key.provider)?;
        p.rename_live(&session, title)
    }

    /// Stored rename: write title slot into the session file.
    pub fn rename_stored(&mut self, key: &SessionKey, title: &str) -> Result<()> {
        let session = self.session_for_key(key);
        let p = self.registry.get_mut(key.provider)?;
        p.rename_stored(&session, title)
    }

    /// Delete a stored session (jsonl + artifacts). Caller must close the live
    /// session first.
    pub fn delete_stored(&mut self, key: &SessionKey) -> Result<()> {
        let session = self.session_for_key(key);
        let p = self.registry.get_mut(key.provider)?;
        p.delete_stored(&session)
    }

    /// Close a live session (if any), join pending kills, then delete stored data.
    pub fn delete_session(&mut self, key: &SessionKey) -> Result<()> {
        if self.is_live(key) {
            self.close_session_blocking(key);
        }
        self.join_pending_kills();
        self.delete_stored(key)?;
        self.known_sessions.remove(key);
        Ok(())
    }

    /// Advance a modified-files scanner using the supervisor's session metadata.
    pub fn advance_modified_files(
        &mut self,
        scan: &mut dyn ModifiedFilesScanner,
        key: &SessionKey,
    ) -> Result<bool> {
        scan.advance(&self.session_for_key(key))
    }

    /// Load transcript blocks for a session.
    pub fn load_transcript(&mut self, key: &SessionKey) -> Result<Vec<TranscriptBlock>> {
        let session = self.session_for_key(key);
        let p = self.registry.get_mut(key.provider)?;
        p.load_transcript(&session)
    }

    /// Get a modified-files scanner for a session.
    pub fn modified_files_scanner(
        &mut self,
        key: &SessionKey,
    ) -> Result<Option<Box<dyn ModifiedFilesScanner>>> {
        let session = self.session_for_key(key);
        let p = self.registry.get_mut(key.provider)?;
        p.modified_files_scanner(&session)
    }

    /// Prefer known disk metadata (path/cwd/title/parent/mtime/size), then live,
    /// then a minimal fallback.
    fn session_for_key(&self, key: &SessionKey) -> ProviderSession {
        if let Some(known) = self.known_sessions.get(key) {
            return known.clone();
        }
        if let Some(entry) = self.live.get(key) {
            return ProviderSession {
                key: key.clone(),
                title: entry.title.clone(),
                title_source: entry.title_source,
                parent_ref: None,
                path: None,
                cwd: entry.cwd.clone(),
                modified_at: entry.spawned_at,
                size: 0,
            };
        }
        ProviderSession {
            key: key.clone(),
            title: String::new(),
            title_source: TitleSource::Fallback,
            parent_ref: None,
            path: None,
            cwd: PathBuf::from("."),
            modified_at: Utc::now(),
            size: 0,
        }
    }

    // --- shutdown ---

    /// Synchronous teardown for final exit — blocks on the graceful
    /// SIGHUP→SIGTERM→SIGKILL ladder per child.
    pub fn shutdown_all_blocking(&mut self) {
        let keys: Vec<SessionKey> = self.live.keys().cloned().collect();
        for key in keys {
            let p = self.registry.get_mut(key.provider).ok();
            if let Some(p) = p {
                p.forget_session(&key);
            }
            if let Some(mut entry) = self.live.remove(&key) {
                entry.pty.kill_process_group();
                drop(entry.lock.take());
            }
        }
        for handle in std::mem::take(&mut self.kill_threads) {
            let _ = handle.join();
        }
    }
}

/// Small mtime skew tolerance: jsonl may be stamped slightly before our spawn clock.
const SYNTHETIC_MTIME_SKEW_MS: i64 = 500;

/// Match a brand-new (non-fork) disk session to a synthetic live spawn.
/// Prefers the oldest eligible file at/after spawn time so concurrent `new-N`
/// entries pair stably instead of both claiming the newest uuid.
fn pick_synthetic_disk_match<'a, F>(
    disk: &'a [ProviderSession],
    spawned_at: DateTime<Utc>,
    is_taken: F,
) -> Option<&'a ProviderSession>
where
    F: Fn(&str) -> bool,
{
    let skew = chrono::Duration::milliseconds(SYNTHETIC_MTIME_SKEW_MS);
    disk.iter()
        .filter(|d| {
            !is_taken(&d.key.session_id)
                && d.parent_ref.is_none()
                && d.modified_at + skew >= spawned_at
        })
        .min_by_key(|d| d.modified_at)
}

/// Wrap [`pick_synthetic_disk_match`] with `SessionKey` provider semantics
/// (e.g. `SessionKey::omp("new-1")` only pairs with OMP disk rows).
fn pick_synthetic_disk_match_for_key<'a, F>(
    disk: &'a [ProviderSession],
    live_key: &SessionKey,
    spawned_at: DateTime<Utc>,
    is_taken: F,
) -> Option<&'a ProviderSession>
where
    F: Fn(&str) -> bool,
{
    let same_provider: Vec<ProviderSession> = disk
        .iter()
        .filter(|d| d.key.provider == live_key.provider)
        .cloned()
        .collect();
    let matched = pick_synthetic_disk_match(&same_provider, spawned_at, is_taken)?;
    disk.iter().find(|d| d.key == matched.key)
}

/// Merge disk title into a live entry without downgrading.
/// Official wins over everything; Provisional wins over Fallback; Fallback(id)
/// must not replace live `New session` while omp jsonl is still half-written.
fn merge_disk_title(
    live_title: &str,
    live_source: TitleSource,
    disk_title: &str,
    disk_source: TitleSource,
) -> Option<(String, TitleSource)> {
    match (live_source, disk_source) {
        (_, TitleSource::Official) => {
            if live_title == disk_title && live_source == TitleSource::Official {
                None
            } else {
                Some((disk_title.to_string(), disk_source))
            }
        }
        (TitleSource::Official, _) => None,
        (_, TitleSource::Provisional) => {
            if live_title == disk_title && live_source == TitleSource::Provisional {
                None
            } else {
                Some((disk_title.to_string(), disk_source))
            }
        }
        (TitleSource::Provisional, TitleSource::Fallback) => None,
        (TitleSource::Fallback, TitleSource::Fallback) => {
            if live_title == "New session" || live_title == disk_title {
                None
            } else {
                Some((disk_title.to_string(), disk_source))
            }
        }
    }
}

fn apply_disk_title_to_live(title: &mut String, source: &mut TitleSource, disk: &ProviderSession) {
    if let Some((next_title, next_source)) =
        merge_disk_title(title, *source, &disk.title, disk.title_source)
    {
        *title = next_title;
        *source = next_source;
    }
}

/// Map a provider [`SpawnSpec`] onto the exact [`PtySession::spawn`] argument set.
fn pty_spawn_params_from_spec(
    spec: &SpawnSpec,
    rows: u16,
    cols: u16,
    kitty: bool,
    surface: HostSurface,
) -> PtySpawnParams<'_> {
    PtySpawnParams {
        program: &spec.program,
        args: &spec.args,
        env: &spec.env,
        cwd: &spec.cwd,
        rows,
        cols,
        kitty,
        surface,
    }
}

/// Resume attach guards: external occupant → flock → available → spawn spec.
fn run_resume_attach_guards<C, L, S>(
    ctx: &mut C,
    check_external: impl FnOnce(&mut C) -> Result<()>,
    acquire_lock: impl FnOnce(&mut C) -> Result<L>,
    available: impl FnOnce(&mut C) -> Result<()>,
    spawn_spec: impl FnOnce(&mut C) -> Result<S>,
) -> Result<(L, S)> {
    check_external(ctx)?;
    let lock = acquire_lock(ctx)?;
    available(ctx)?;
    let spec = spawn_spec(ctx)?;
    Ok((lock, spec))
}

/// Write PTY bytes for a live rename. Once for [`LiveRenameAction::WritePty`],
/// zero times for [`LiveRenameAction::Persisted`].
#[cfg(test)]
fn write_live_rename_action(action: &LiveRenameAction, mut write: impl FnMut(&[u8])) -> usize {
    match action {
        LiveRenameAction::WritePty(bytes) => {
            write(bytes);
            1
        }
        LiveRenameAction::Persisted => 0,
    }
}

/// Delete order: close live (if any) → join pending kills → provider delete.
#[cfg(test)]
fn run_delete_session_steps(is_live: bool, log: &mut Vec<&'static str>) {
    if is_live {
        log.push("close");
    }
    log.push("join");
    log.push("delete");
}

/// Remap focused / selected keys across a rebind list.
#[cfg(test)]
fn remap_optional_key(slot: &mut Option<SessionKey>, rebinds: &[(SessionKey, SessionKey)]) {
    if let Some(cur) = slot.as_ref() {
        if let Some((_, new)) = rebinds.iter().find(|(old, _)| old == cur) {
            *slot = Some(new.clone());
        }
    }
}

/// Remap unread flags so they follow the new key after a rebind.
#[cfg(test)]
fn remap_unread_keys<V>(map: &mut HashMap<SessionKey, V>, rebinds: &[(SessionKey, SessionKey)]) {
    for (old, new) in rebinds {
        if let Some(v) = map.remove(old) {
            map.insert(new.clone(), v);
        }
    }
}

#[cfg(test)]
fn remap_unread_set(set: &mut HashSet<SessionKey>, rebinds: &[(SessionKey, SessionKey)]) {
    for (old, new) in rebinds {
        if set.remove(old) {
            set.insert(new.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_support::{session_on, FakeProvider};
    use anyhow::anyhow;
    use std::time::Instant;

    fn disk(
        id: &str,
        title: &str,
        source: TitleSource,
        mtime: DateTime<Utc>,
        parent: Option<&str>,
    ) -> ProviderSession {
        ProviderSession {
            key: SessionKey::omp(id),
            title: title.into(),
            title_source: source,
            parent_ref: parent.map(|s| s.into()),
            path: Some(PathBuf::from(format!("/tmp/{id}.jsonl"))),
            cwd: PathBuf::from("/tmp"),
            modified_at: mtime,
            size: 1,
        }
    }

    fn supervisor_with(providers: Vec<FakeProvider>, default: ProviderId) -> SessionSupervisor {
        let mut reg = ProviderRegistry::empty_for_test(default);
        for p in providers {
            reg.register(Box::new(p)).unwrap();
        }
        SessionSupervisor::from_registry_for_test(reg)
    }

    #[test]
    fn merge_keeps_new_session_over_fallback_id() {
        assert_eq!(
            merge_disk_title(
                "New session",
                TitleSource::Fallback,
                "abc-uuid",
                TitleSource::Fallback
            ),
            None
        );
    }

    #[test]
    fn merge_upgrades_to_provisional_and_official() {
        assert_eq!(
            merge_disk_title(
                "New session",
                TitleSource::Fallback,
                "hello",
                TitleSource::Provisional
            ),
            Some(("hello".into(), TitleSource::Provisional))
        );
        assert_eq!(
            merge_disk_title(
                "hello",
                TitleSource::Provisional,
                "Renamed",
                TitleSource::Official
            ),
            Some(("Renamed".into(), TitleSource::Official))
        );
    }

    #[test]
    fn merge_does_not_downgrade_official() {
        assert_eq!(
            merge_disk_title(
                "Renamed",
                TitleSource::Official,
                "abc-uuid",
                TitleSource::Fallback
            ),
            None
        );
        assert_eq!(
            merge_disk_title(
                "Renamed",
                TitleSource::Official,
                "older msg",
                TitleSource::Provisional
            ),
            None
        );
    }

    #[test]
    fn synthetic_match_skips_fork_and_pairs_oldest() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(1);
        let t2 = t0 + chrono::Duration::seconds(2);
        let sessions = vec![
            disk("fork", "f", TitleSource::Fallback, t2, Some("parent")),
            disk("new-b", "b", TitleSource::Fallback, t2, None),
            disk("new-a", "a", TitleSource::Fallback, t1, None),
        ];
        let first = pick_synthetic_disk_match(&sessions, t0, |_| false).unwrap();
        assert_eq!(first.key.session_id, "new-a");
        let second = pick_synthetic_disk_match(&sessions, t0, |id| id == "new-a").unwrap();
        assert_eq!(second.key.session_id, "new-b");
        assert!(
            pick_synthetic_disk_match(&sessions, t0, |id| id == "new-a" || id == "new-b").is_none()
        );
    }

    #[test]
    fn synthetic_match_allows_small_mtime_skew() {
        let spawned = Utc::now();
        let slightly_earlier = spawned - chrono::Duration::milliseconds(200);
        let sessions = vec![disk(
            "id1",
            "id1",
            TitleSource::Fallback,
            slightly_earlier,
            None,
        )];
        assert_eq!(
            pick_synthetic_disk_match(&sessions, spawned, |_| false)
                .map(|d| d.key.session_id.as_str()),
            Some("id1")
        );
    }

    #[test]
    fn session_key_distinguishes_same_id_across_providers() {
        let omp_key = SessionKey::omp("x");
        let fake_key = SessionKey::new(ProviderId::new("fake"), "x");
        assert_ne!(omp_key, fake_key);
        let mut map = HashMap::new();
        map.insert(omp_key.clone(), 1);
        map.insert(fake_key.clone(), 2);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn supervisor_uses_provider_qualified_live_keys() {
        let omp = ProviderId::OMP;
        let fake_id = ProviderId::new("fake");
        let t = Utc::now();
        let (omp_p, omp_state) = FakeProvider::new(omp);
        let (fake_p, fake_state) = FakeProvider::new(fake_id);
        omp_state.lock().unwrap().sessions = vec![session_on(
            omp,
            "x",
            "/ws/omp",
            "omp-x",
            TitleSource::Fallback,
            None,
            t,
            1,
        )];
        fake_state.lock().unwrap().sessions = vec![session_on(
            fake_id,
            "x",
            "/ws/fake",
            "fake-x",
            TitleSource::Fallback,
            None,
            t,
            1,
        )];
        let mut sup = supervisor_with(vec![omp_p, fake_p], omp);

        let omp_list = sup
            .select_provider_workspace(omp, "ws-omp", Some(Path::new("/ws/omp")))
            .unwrap();
        let fake_list = sup
            .select_provider_workspace(fake_id, "ws-fake", Some(Path::new("/ws/fake")))
            .unwrap();

        assert_eq!(omp_list.len(), 1);
        assert_eq!(fake_list.len(), 1);
        assert_ne!(omp_list[0].key, fake_list[0].key);
        assert_eq!(omp_list[0].key, SessionKey::omp("x"));
        assert_eq!(fake_list[0].key, SessionKey::new(fake_id, "x"));

        let mut map = HashMap::new();
        map.insert(omp_list[0].key.clone(), "omp");
        map.insert(fake_list[0].key.clone(), "fake");
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&SessionKey::omp("x")), Some(&"omp"));
        assert_eq!(map.get(&SessionKey::new(fake_id, "x")), Some(&"fake"));

        // Both remain addressable; neither overwrote the other.
        assert_eq!(
            sup.session_for_key(&SessionKey::omp("x")).path,
            Some(PathBuf::from("/tmp/omp/x.jsonl"))
        );
        assert_eq!(
            sup.session_for_key(&SessionKey::new(fake_id, "x")).path,
            Some(PathBuf::from("/tmp/fake/x.jsonl"))
        );
    }

    #[test]
    fn supervisor_preserves_omp_synthetic_reconcile() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(1);
        let t2 = t0 + chrono::Duration::seconds(2);
        let fake_id = ProviderId::new("fake");
        let sessions = vec![
            session_on(
                fake_id,
                "older-fake",
                "/tmp",
                "fake",
                TitleSource::Fallback,
                None,
                t0,
                1,
            ),
            disk("fork", "f", TitleSource::Fallback, t1, Some("parent")),
            disk("new-b", "b", TitleSource::Fallback, t2, None),
            disk("new-a", "a", TitleSource::Fallback, t1, None),
        ];
        let live = SessionKey::omp("new-1");
        let first = pick_synthetic_disk_match_for_key(&sessions, &live, t0, |_| false).unwrap();
        assert_eq!(first.key, SessionKey::omp("new-a"));
        assert_eq!(first.key.provider, ProviderId::OMP);
        let second =
            pick_synthetic_disk_match_for_key(&sessions, &live, t0, |id| id == "new-a").unwrap();
        assert_eq!(second.key, SessionKey::omp("new-b"));
        assert!(
            pick_synthetic_disk_match_for_key(&sessions, &live, t0, |id| {
                id == "new-a" || id == "new-b"
            })
            .is_none()
        );
    }

    #[test]
    fn supervisor_rebind_preserves_provider_id() {
        let omp = ProviderId::OMP;
        let fake_id = ProviderId::new("fake");
        let t = Utc::now();
        let (omp_p, omp_state) = FakeProvider::new(omp);
        let (fake_p, fake_state) = FakeProvider::new(fake_id);
        omp_state.lock().unwrap().capabilities.live_rebind = true;
        // Child on OMP whose parent_ref points at an id that exists on *fake*.
        omp_state.lock().unwrap().sessions = vec![session_on(
            omp,
            "child",
            "/ws/omp",
            "child",
            TitleSource::Fallback,
            Some("shared-id"),
            t,
            1,
        )];
        fake_state.lock().unwrap().sessions = vec![session_on(
            fake_id,
            "shared-id",
            "/ws/fake",
            "other",
            TitleSource::Fallback,
            None,
            t,
            1,
        )];
        let mut sup = supervisor_with(vec![omp_p, fake_p], omp);

        let fake_list = sup
            .select_provider_workspace(fake_id, "ws-fake", Some(Path::new("/ws/fake")))
            .unwrap();
        let omp_list = sup
            .select_provider_workspace(omp, "ws-omp", Some(Path::new("/ws/omp")))
            .unwrap();

        assert_eq!(fake_list[0].key, SessionKey::new(fake_id, "shared-id"));
        assert_eq!(omp_list[0].key, SessionKey::omp("child"));
        assert_eq!(omp_list[0].key.provider, ProviderId::OMP);
        assert_eq!(fake_list[0].key.provider, fake_id);
        assert!(sup.drain_rebinds().is_empty());
        // Other provider's identity is untouched.
        assert_eq!(
            sup.session_for_key(&SessionKey::new(fake_id, "shared-id"))
                .key
                .provider,
            fake_id
        );
    }

    #[test]
    fn supervisor_builds_pty_from_spawn_spec() {
        let spec = SpawnSpec {
            program: "omp".into(),
            args: vec!["--cwd".into(), "/ws".into()],
            env: vec![("PI_TUI_SYNC_OUTPUT".into(), "1".into())],
            cwd: PathBuf::from("/ws"),
        };
        let surface = HostSurface::fallback(crate::appearance::Appearance::Dark);
        let params = pty_spawn_params_from_spec(&spec, 24, 80, true, surface);
        assert_eq!(params.program, spec.program);
        assert_eq!(params.args, spec.args.as_slice());
        assert_eq!(params.env, spec.env.as_slice());
        assert_eq!(params.cwd, spec.cwd.as_path());
        assert_eq!(params.rows, 24);
        assert_eq!(params.cols, 80);
        assert!(params.kitty);
        assert_eq!(params.surface, surface);
        // Equal to what PtySession::spawn would receive — do not spawn a PTY.
    }

    #[test]
    fn supervisor_preserves_resume_guard_order() {
        let mut log: Vec<String> = Vec::new();
        let ok = run_resume_attach_guards(
            &mut log,
            |l| {
                l.push("external".into());
                Ok(())
            },
            |l| {
                l.push("flock".into());
                Ok("lock")
            },
            |l| {
                l.push("available".into());
                Ok(())
            },
            |l| {
                l.push("spawn_spec".into());
                Ok("spec")
            },
        )
        .unwrap();
        assert_eq!(ok, ("lock", "spec"));
        assert_eq!(log, ["external", "flock", "available", "spawn_spec"]);

        let mut log: Vec<String> = Vec::new();
        let err = run_resume_attach_guards(
            &mut log,
            |l| {
                l.push("external".into());
                Err(anyhow!("occupied"))
            },
            |l| {
                l.push("flock".into());
                Ok("lock")
            },
            |l| {
                l.push("available".into());
                Ok(())
            },
            |l| {
                l.push("spawn_spec".into());
                Ok("spec")
            },
        );
        assert!(err.is_err());
        assert_eq!(log, ["external"]);
    }

    #[test]
    fn supervisor_disables_parent_rebind_when_capability_is_false() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        state.lock().unwrap().capabilities.live_rebind = false;
        state.lock().unwrap().sessions = vec![
            session_on(
                omp,
                "parent",
                "/ws",
                "parent",
                TitleSource::Fallback,
                None,
                t,
                1,
            ),
            session_on(
                omp,
                "child",
                "/ws",
                "child",
                TitleSource::Fallback,
                Some("parent"),
                t + chrono::Duration::seconds(1),
                1,
            ),
        ];
        let mut sup = supervisor_with(vec![p], omp);
        let list = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|s| s.key == SessionKey::omp("parent")));
        assert!(list.iter().any(|s| s.key == SessionKey::omp("child")));
        assert!(sup.drain_rebinds().is_empty());
        let calls = state.lock().unwrap().calls.clone();
        assert!(
            !calls.iter().any(|c| c.starts_with("parent_refers_to")),
            "parent_refers_to must not be called when live_rebind=false: {calls:?}"
        );

        // Synthetic adopt stays independent of the live_rebind gate.
        let sessions = state.lock().unwrap().sessions.clone();
        let syn = pick_synthetic_disk_match_for_key(
            &sessions,
            &SessionKey::omp("new-1"),
            t - chrono::Duration::seconds(1),
            |_| false,
        )
        .unwrap();
        assert_eq!(syn.key, SessionKey::omp("parent"));
        assert!(syn.parent_ref.is_none());
    }

    #[test]
    fn supervisor_normalizes_title_before_live_state_checks() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        state.lock().unwrap().sessions = vec![session_on(
            omp,
            "s1",
            "/ws",
            "old",
            TitleSource::Fallback,
            None,
            t,
            1,
        )];
        let mut sup = supervisor_with(vec![p], omp);
        sup.select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        state.lock().unwrap().calls.clear();

        for draft in ["\n\n", "   ", "\t\n  ", "\u{0007}\u{0008}", "  \nworld"] {
            assert!(
                sup.normalize_title(draft).is_err(),
                "expected empty-title error for {draft:?}"
            );
        }
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.iter().all(|c| c == "normalize_title"), "{calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("rename_live")));
        assert!(!calls.iter().any(|c| c.starts_with("rename_stored")));

        let good = sup.normalize_title("  hello\nworld\u{0007}").unwrap();
        assert_eq!(good, "hello");
        let again = sup.normalize_title("  hello\nworld\u{0007}").unwrap();
        assert_eq!(again, good);

        let key = SessionKey::omp("s1");
        sup.rename_live(&key, &good).unwrap();
        sup.rename_stored(&key, &good).unwrap();
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.iter().any(|c| c == "rename_live:hello"));
        assert!(calls.iter().any(|c| c == "rename_stored:hello"));
    }

    #[test]
    fn supervisor_workspace_switch_discards_old_provider_events() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        let sess_a = session_on(omp, "a", "/ws/a", "A", TitleSource::Official, None, t, 10);
        let sess_b = session_on(omp, "b", "/ws/b", "B", TitleSource::Official, None, t, 10);
        state.lock().unwrap().sessions = vec![sess_a.clone()];
        let mut sup = supervisor_with(vec![p], omp);
        sup.select_provider_workspace(omp, "ws-a", Some(Path::new("/ws/a")))
            .unwrap();
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(sess_a.clone()));

        state.lock().unwrap().sessions = vec![sess_b.clone()];
        // select B must clear the provider queue (FakeProvider.select_workspace).
        let list_b = sup
            .select_provider_workspace(omp, "ws-b", Some(Path::new("/ws/b")))
            .unwrap();
        assert_eq!(list_b.len(), 1);
        assert_eq!(list_b[0].key, SessionKey::omp("b"));
        assert!(state.lock().unwrap().queued_changes.is_empty());

        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert!(
            changes.iter().all(|c| match c {
                SessionSummaryChange::Upsert { summary, .. } => summary.key.session_id != "a",
                SessionSummaryChange::ReplaceAll { sessions, .. } => {
                    sessions.iter().all(|s| s.key.session_id != "a")
                }
            }),
            "poll must not emit A's session after switch: {changes:?}"
        );

        // If a stale Upsert with cwd A is returned anyway, supervisor drops it.
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(sess_a));
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert!(
            changes.is_empty(),
            "stale cwd-A upsert must be dropped: {changes:?}"
        );
    }

    #[test]
    fn supervisor_provider_upsert_preserves_title_busy_unread_pipeline() {
        let omp = ProviderId::OMP;
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(5);
        let (p, state) = FakeProvider::new(omp);
        let sess = session_on(
            omp,
            "s1",
            "/ws",
            "old-title",
            TitleSource::Provisional,
            None,
            t0,
            10,
        );
        state.lock().unwrap().sessions = vec![sess.clone()];
        state.lock().unwrap().busy = true;
        let mut sup = supervisor_with(vec![p], omp);
        let initial = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(initial.len(), 1);
        assert!(initial[0].agent_busy);

        let mut updated = sess.clone();
        updated.title = "official-title".into();
        updated.title_source = TitleSource::Official;
        updated.modified_at = t1;
        updated.size = 99;
        state.lock().unwrap().busy = false;
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(updated));

        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            SessionSummaryChange::Upsert {
                summary,
                became_idle,
            } => {
                assert!(*became_idle, "became_idle must be true exactly once");
                assert_eq!(summary.title, "official-title");
                assert_eq!(summary.title_source, TitleSource::Official);
                assert_eq!(summary.mtime, t1);
                assert_eq!(summary.size, 99);
                assert!(!summary.agent_busy);
            }
            other => panic!("expected Upsert, got {other:?}"),
        }

        // Second poll of the same known session: became_idle is not true again.
        let mut again = sess;
        again.title = "official-title".into();
        again.title_source = TitleSource::Official;
        again.modified_at = t1;
        again.size = 99;
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(again));
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        match &changes[0] {
            SessionSummaryChange::Upsert { became_idle, .. } => {
                assert!(!*became_idle);
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_live_rename_writes_provider_action_bytes() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        state.lock().unwrap().sessions = vec![session_on(
            omp,
            "s1",
            "/ws",
            "old",
            TitleSource::Fallback,
            None,
            t,
            1,
        )];
        state.lock().unwrap().rename_live_action = Some(LiveRenameAction::WritePty(
            b"\x15/rename new title\r".to_vec(),
        ));
        let mut sup = supervisor_with(vec![p], omp);
        sup.select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        let action = sup
            .rename_live(&SessionKey::omp("s1"), "new title")
            .unwrap();
        let mut writes = Vec::new();
        let n = write_live_rename_action(&action, |b| writes.push(b.to_vec()));
        assert_eq!(n, 1);
        assert_eq!(writes, vec![b"\x15/rename new title\r".to_vec()]);

        let n = write_live_rename_action(&LiveRenameAction::Persisted, |_| {
            panic!("Persisted must not write")
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn supervisor_delete_closes_live_before_provider_delete() {
        let mut log = Vec::new();
        run_delete_session_steps(true, &mut log);
        assert_eq!(log, ["close", "join", "delete"]);

        let mut log = Vec::new();
        run_delete_session_steps(false, &mut log);
        assert_eq!(log, ["join", "delete"]);
    }

    #[test]
    fn supervisor_rebind_preserves_unread_and_selection_keys() {
        let old = SessionKey::omp("old");
        let new = SessionKey::omp("new");
        let rebinds = vec![(old.clone(), new.clone())];

        let mut focused = Some(old.clone());
        let mut selected = Some(old.clone());
        remap_optional_key(&mut focused, &rebinds);
        remap_optional_key(&mut selected, &rebinds);
        assert_eq!(focused, Some(new.clone()));
        assert_eq!(selected, Some(new.clone()));

        let mut unread_set = HashSet::from([old.clone()]);
        remap_unread_set(&mut unread_set, &rebinds);
        assert!(unread_set.contains(&new));
        assert!(!unread_set.contains(&old));

        let mut unread_map = HashMap::from([(old.clone(), true)]);
        remap_unread_keys(&mut unread_map, &rebinds);
        assert_eq!(unread_map.get(&new), Some(&true));
        assert!(!unread_map.contains_key(&old));
    }

    #[test]
    fn supervisor_select_workspace_none_clears_monitoring() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        let sess = session_on(omp, "s1", "/ws", "s1", TitleSource::Fallback, None, t, 1);
        state.lock().unwrap().sessions = vec![sess.clone()];
        let mut sup = supervisor_with(vec![p], omp);

        let list = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(list.len(), 1);

        let cleared = sup.select_provider_workspace(omp, "ws", None).unwrap();
        assert!(cleared.is_empty());

        // select_workspace already cleared the queue; push AFTER None.
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(sess));
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert!(
            changes.is_empty(),
            "poll must stay empty when current_watch is None: {changes:?}"
        );
        assert_eq!(
            state.lock().unwrap().queued_changes.len(),
            1,
            "poll must not call poll_changes after None select"
        );
    }

    #[test]
    fn supervisor_poll_without_watch_returns_empty() {
        let omp = ProviderId::OMP;
        let (p, _state) = FakeProvider::new(omp);
        let mut sup = supervisor_with(vec![p], omp);
        assert!(sup
            .poll_provider_changes(Instant::now())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn supervisor_session_for_key_fallback_is_minimal() {
        let omp = ProviderId::OMP;
        let (p, state) = FakeProvider::new(omp);
        let mut sup = supervisor_with(vec![p], omp);
        sup.rename_stored(&SessionKey::omp("unknown-id"), "t")
            .unwrap();
        let last = state.lock().unwrap().last_session.clone().unwrap();
        assert_eq!(last.key, SessionKey::omp("unknown-id"));
        assert_eq!(last.title, "");
        assert_eq!(last.title_source, TitleSource::Fallback);
        assert_eq!(last.path, None);
        assert_eq!(last.cwd, PathBuf::from("."));
        assert_eq!(last.size, 0);
    }

    #[test]
    fn merge_disk_title_upgrades_custom_fallback() {
        assert_eq!(
            merge_disk_title(
                "custom-fallback",
                TitleSource::Fallback,
                "abc-uuid",
                TitleSource::Fallback
            ),
            Some(("abc-uuid".into(), TitleSource::Fallback))
        );
        assert_eq!(
            merge_disk_title(
                "New session",
                TitleSource::Fallback,
                "abc-uuid",
                TitleSource::Fallback
            ),
            None
        );
        assert_eq!(
            merge_disk_title(
                "abc-uuid",
                TitleSource::Fallback,
                "abc-uuid",
                TitleSource::Fallback
            ),
            None
        );
    }

    #[test]
    fn supervisor_first_scan_seeds_without_adopting_old_forks() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        state.lock().unwrap().capabilities.live_rebind = true;
        state.lock().unwrap().sessions = vec![
            session_on(
                omp,
                "parent",
                "/ws",
                "parent",
                TitleSource::Fallback,
                None,
                t,
                1,
            ),
            session_on(
                omp,
                "child",
                "/ws",
                "child",
                TitleSource::Fallback,
                Some("parent"),
                t + chrono::Duration::seconds(1),
                1,
            ),
        ];
        let mut sup = supervisor_with(vec![p], omp);
        let list = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(list.len(), 2);
        assert!(sup.drain_rebinds().is_empty());
    }

    #[test]
    fn supervisor_close_workspace_resets_known_disk_ids() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        state.lock().unwrap().capabilities.live_rebind = true;
        state.lock().unwrap().sessions = vec![
            session_on(
                omp,
                "parent",
                "/ws",
                "parent",
                TitleSource::Fallback,
                None,
                t,
                1,
            ),
            session_on(
                omp,
                "child",
                "/ws",
                "child",
                TitleSource::Fallback,
                Some("parent"),
                t + chrono::Duration::seconds(1),
                1,
            ),
        ];
        let mut sup = supervisor_with(vec![p], omp);
        let first = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(first.len(), 2);
        assert!(sup.drain_rebinds().is_empty());
        assert!(sup.known_disk_ids_contains("ws"));

        assert_eq!(sup.close_workspace_sessions("ws"), 0);
        assert_eq!(
            sup.known_disk_ids_contains("ws"),
            false,
            "close must drop known_disk_ids so the next select first-scans"
        );

        let second = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(second.len(), 2);
        assert!(sup.drain_rebinds().is_empty());
        assert!(sup.known_disk_ids_contains("ws"));
    }

    #[test]
    fn supervisor_rescan_reports_became_idle() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        state.lock().unwrap().sessions = vec![session_on(
            omp,
            "s1",
            "/ws",
            "s1",
            TitleSource::Fallback,
            None,
            t,
            1,
        )];
        state.lock().unwrap().busy = true;
        let mut sup = supervisor_with(vec![p], omp);
        let initial = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(initial.len(), 1);
        assert!(initial[0].agent_busy);

        state.lock().unwrap().busy = false;
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Rescan);
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            SessionSummaryChange::ReplaceAll { became_idle, .. } => {
                assert!(
                    became_idle.contains(&SessionKey::omp("s1")),
                    "first rescan must report s1 idle: {became_idle:?}"
                );
            }
            other => panic!("expected ReplaceAll, got {other:?}"),
        }

        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Rescan);
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        match &changes[0] {
            SessionSummaryChange::ReplaceAll { became_idle, .. } => {
                assert!(
                    !became_idle.contains(&SessionKey::omp("s1")),
                    "second rescan must not re-report s1: {became_idle:?}"
                );
            }
            other => panic!("expected ReplaceAll, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_unknown_upsert_triggers_replace_all() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        let s1 = session_on(omp, "s1", "/ws", "s1", TitleSource::Fallback, None, t, 1);
        let s2 = session_on(omp, "s2", "/ws", "s2", TitleSource::Fallback, None, t, 1);
        state.lock().unwrap().sessions = vec![s1.clone()];
        let mut sup = supervisor_with(vec![p], omp);
        let initial = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].key, SessionKey::omp("s1"));

        state.lock().unwrap().sessions = vec![s1, s2.clone()];
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(s2));

        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            SessionSummaryChange::ReplaceAll { sessions, .. } => {
                assert!(
                    sessions.iter().any(|s| s.key == SessionKey::omp("s1")),
                    "ReplaceAll must include s1: {sessions:?}"
                );
                assert!(
                    sessions.iter().any(|s| s.key == SessionKey::omp("s2")),
                    "ReplaceAll must include s2: {sessions:?}"
                );
            }
            other => panic!("expected ReplaceAll, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_removed_change_rescans_list() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        let s1 = session_on(omp, "s1", "/ws", "s1", TitleSource::Fallback, None, t, 1);
        let s2 = session_on(omp, "s2", "/ws", "s2", TitleSource::Fallback, None, t, 1);
        state.lock().unwrap().sessions = vec![s1.clone(), s2];
        let mut sup = supervisor_with(vec![p], omp);
        let initial = sup
            .select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();
        assert_eq!(initial.len(), 2);

        state.lock().unwrap().sessions = vec![s1];
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Removed(SessionKey::omp("s2")));

        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            SessionSummaryChange::ReplaceAll { sessions, .. } => {
                assert!(
                    sessions.iter().any(|s| s.key == SessionKey::omp("s1")),
                    "ReplaceAll must contain s1: {sessions:?}"
                );
                assert!(
                    sessions.iter().all(|s| s.key != SessionKey::omp("s2")),
                    "ReplaceAll must not contain s2: {sessions:?}"
                );
            }
            other => panic!("expected ReplaceAll, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_drops_upsert_from_other_provider() {
        let omp = ProviderId::OMP;
        let t = Utc::now();
        let (p, state) = FakeProvider::new(omp);
        let s1 = session_on(omp, "s1", "/ws", "s1", TitleSource::Fallback, None, t, 1);
        state.lock().unwrap().sessions = vec![s1.clone()];
        let mut sup = supervisor_with(vec![p], omp);
        sup.select_provider_workspace(omp, "ws", Some(Path::new("/ws")))
            .unwrap();

        let foreign = session_on(
            ProviderId::new("codex"),
            "s1",
            "/ws",
            "s1",
            TitleSource::Fallback,
            None,
            t,
            1,
        );
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(foreign));
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert!(
            changes.is_empty(),
            "upsert from other provider must be dropped: {changes:?}"
        );

        let mut other_cwd = s1;
        other_cwd.cwd = PathBuf::from("/other");
        state
            .lock()
            .unwrap()
            .queued_changes
            .push(ProviderChange::Upsert(other_cwd));
        let changes = sup.poll_provider_changes(Instant::now()).unwrap();
        assert!(
            changes.is_empty(),
            "upsert with other cwd must be dropped: {changes:?}"
        );
    }
}

impl Drop for SessionSupervisor {
    fn drop(&mut self) {
        self.shutdown_all_blocking();
    }
}
