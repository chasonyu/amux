//! SessionSupervisor: disk discovery + live PtySession map.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};

use crate::config::AmuxConfig;
use crate::lock::{check_occupiable, SessionLock};
use crate::provider::omp::{parent_refers_to, OmpDiskSession, OmpProvider, TitleKind};
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

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub workspace_id: String,
    pub provider: &'static str,
    pub title: String,
    pub title_kind: TitleKind,
    /// True when omp header has `parentSession` (fork / file branch).
    pub is_fork: bool,
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub mtime: DateTime<Utc>,
    pub size: u64,
    pub live: bool,
    pub status: SessionStatus,
}

struct LiveEntry {
    pty: PtySession,
    /// Released once the child exits; held while alive to prevent concurrent attach.
    lock: Option<SessionLock>,
    status: SessionStatus,
    title: String,
    title_kind: TitleKind,
    cwd: PathBuf,
    workspace_id: String,
    /// When the session was spawned — used to match "new-N" entries to
    /// omp's on-disk uuid after omp writes the session file.
    spawned_at: DateTime<Utc>,
}

pub struct SessionSupervisor {
    provider: OmpProvider,
    config: AmuxConfig,
    live: HashMap<String, LiveEntry>,
    /// Synthetic ids for brand-new sessions not yet on disk.
    next_new: u64,
    /// Detached kill threads from close_session — joined in shutdown to
    /// prevent orphaning when amux exits mid-ladder. (§4.2.8 / E14)
    kill_threads: Vec<JoinHandle<()>>,
    /// Whether the outer terminal supports kitty keyboard protocol.
    /// Passed to PtySession to set the VT's kitty_keyboard config. (§4.2.3a.2)
    kitty_keyboard: bool,
    /// Last-seen disk session ids per workspace — used to detect newly created
    /// fork/branch children without mistaking historical siblings for rebinds.
    known_disk_ids: HashMap<String, HashSet<String>>,
    /// `(old_id, new_id)` from the latest `list_for_workspace` pass.
    pending_rebinds: Vec<(String, String)>,
}

impl SessionSupervisor {

    pub fn new(config: AmuxConfig, kitty_keyboard: bool) -> Self {
        let provider = OmpProvider::from_config(&config);
        Self {
            provider,
            config,
            live: HashMap::new(),
            next_new: 1,
            kill_threads: Vec::new(),
            kitty_keyboard,
            known_disk_ids: HashMap::new(),
            pending_rebinds: Vec::new(),
        }
    }

    /// Drain omp in-process session rebinds (fork / file-creating branch).
    pub fn drain_rebinds(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.pending_rebinds)
    }

    pub fn provider(&self) -> &OmpProvider {
        &self.provider
    }

    pub fn list_for_workspace(
        &mut self,
        workspace_id: &str,
        cwd: &Path,
    ) -> Result<Vec<SessionSummary>> {
        self.pending_rebinds.clear();
        let disk = self.provider.list_sessions(cwd)?;

        // Reconcile "new-N" live sessions: if omp has written a session file
        // with its own uuid, adopt that uuid as the live key so the sidebar
        // shows one entry, not two, and occupied-detection works. (§5.2)
        let synthetic_ids: Vec<String> = self
            .live
            .iter()
            .filter(|(id, e)| {
                id.starts_with("new-")
                    && e.workspace_id == workspace_id
            })
            .map(|(id, _)| id.clone())
            .collect();
        for syn_id in &synthetic_ids {
            let spawned_at = self.live.get(syn_id).map(|e| e.spawned_at);
            let Some(spawned_at) = spawned_at else { continue };
            // Find a disk session whose uuid is NOT already live and whose
            // mtime is at or after spawn time.
            let matched = disk.iter().find(|d| {
                !self.live.contains_key(&d.id) && d.mtime >= spawned_at
            });
            if let Some(d) = matched {
                let uuid = d.id.clone();
                if let Some(mut entry) = self.live.remove(syn_id) {
                    // Release the old "new-N" lock and acquire under uuid.
                    drop(entry.lock.take());
                    let lock = SessionLock::try_acquire(&uuid).ok();
                    entry.lock = lock;
                    entry.title = d.title.clone();
                    entry.title_kind = d.title_kind;
                    self.live.insert(uuid.clone(), entry);
                    self.pending_rebinds
                        .push((syn_id.clone(), uuid));
                }
            }
        }

        // omp /fork and file-creating /branch rebind the same PTY to a new
        // JSONL; migrate our live map + notify the UI via pending_rebinds.
        self.reconcile_fork_rebinds(workspace_id, &disk);

        // Keep live titles in sync with disk (LLM /rename / firstMessage).
        for d in &disk {
            if let Some(entry) = self.live.get_mut(&d.id) {
                entry.title = d.title.clone();
                entry.title_kind = d.title_kind;
            }
        }

        let mut out: Vec<SessionSummary> = disk
            .into_iter()
            .map(|d| self.to_summary(workspace_id, cwd, d))
            .collect();

        // Include live "new" sessions not yet on disk list
        for (id, entry) in &self.live {
            if entry.workspace_id == workspace_id && !out.iter().any(|s| s.id == *id) {
                out.insert(
                    0,
                    SessionSummary {
                        id: id.clone(),
                        workspace_id: workspace_id.to_string(),
                        provider: "omp",
                        title: entry.title.clone(),
                        title_kind: entry.title_kind,
                        is_fork: false,
                        path: None,
                        cwd: entry.cwd.clone(),
                        mtime: Utc::now(),
                        size: 0,
                        live: true,
                        status: entry.status,
                    },
                );
            }
        }
        Ok(out)
    }

    fn to_summary(&self, workspace_id: &str, cwd: &Path, d: OmpDiskSession) -> SessionSummary {
        let live = self.live.contains_key(&d.id);
        let status = if let Some(e) = self.live.get(&d.id) {
            e.status
        } else {
            SessionStatus::Disk
        };
        SessionSummary {
            id: d.id,
            workspace_id: workspace_id.to_string(),
            provider: "omp",
            title: d.title,
            title_kind: d.title_kind,
            is_fork: d.parent_session.is_some(),
            path: Some(d.path),
            cwd: cwd.to_path_buf(),
            mtime: d.mtime,
            size: d.size,
            live,
            status,
        }
    }

    /// Apply a single-file title refresh into a live entry (if present).
    pub fn apply_disk_title(&mut self, d: &OmpDiskSession) {
        if let Some(entry) = self.live.get_mut(&d.id) {
            entry.title = d.title.clone();
            entry.title_kind = d.title_kind;
        }
    }

    /// When a new disk session appears whose `parentSession` points at a live
    /// session, omp has forked/branched in-process — move the PTY binding.
    fn reconcile_fork_rebinds(&mut self, workspace_id: &str, disk: &[OmpDiskSession]) {
        let current_ids: HashSet<String> = disk.iter().map(|d| d.id.clone()).collect();

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

        let mut newcomers: Vec<&OmpDiskSession> = disk
            .iter()
            .filter(|d| new_ids.contains(&d.id))
            .collect();
        // Newest first so a burst of creates still prefers the latest child.
        newcomers.sort_by(|a, b| b.mtime.cmp(&a.mtime));

        let mut migrated_parents: HashSet<String> = HashSet::new();
        for child in newcomers {
            let Some(parent) = child.parent_session.as_deref() else {
                continue;
            };
            if self.live.contains_key(&child.id) {
                continue;
            }
            let parent_live = self
                .live
                .iter()
                .find(|(live_id, entry)| {
                    entry.workspace_id == workspace_id
                        && !live_id.starts_with("new-")
                        && entry.status != SessionStatus::Exited
                        && parent_refers_to(parent, live_id)
                })
                .map(|(id, _)| id.clone());
            let Some(old_id) = parent_live else {
                continue;
            };
            if migrated_parents.contains(&old_id) {
                continue;
            }
            if self.rebind_live(&old_id, child) {
                migrated_parents.insert(old_id);
            }
        }

        self.known_disk_ids
            .insert(workspace_id.to_string(), current_ids);
    }

    fn rebind_live(&mut self, old_id: &str, child: &OmpDiskSession) -> bool {
        let Some(mut entry) = self.live.remove(old_id) else {
            return false;
        };
        drop(entry.lock.take());
        entry.lock = SessionLock::try_acquire(&child.id).ok();
        entry.title = child.title.clone();
        entry.title_kind = child.title_kind;
        self.live.insert(child.id.clone(), entry);
        self.pending_rebinds
            .push((old_id.to_string(), child.id.clone()));
        tracing::info!(
            target: "amux",
            old = %old_id,
            new = %child.id,
            "omp session rebind (fork/branch)"
        );
        true
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut PtySession> {
        self.live.get_mut(id).map(|e| &mut e.pty)
    }

    pub fn get(&self, id: &str) -> Option<&PtySession> {
        self.live.get(id).map(|e| &e.pty)
    }

    pub fn is_live(&self, id: &str) -> bool {
        self.live.contains_key(id)
    }

    /// Lazy spawn / attach resume. Returns session id.
    pub fn attach_resume(
        &mut self,
        workspace_id: &str,
        cwd: &Path,
        session_id: &str,
        title: &str,
        title_kind: TitleKind,
        rows: u16,
        cols: u16,
    ) -> Result<String> {
        // If the session is live but exited, remove the dead entry and
        // respawn via `omp --resume`. Without this, re-selecting an exited
        // session returns the dead PtySession — the UI advertises "Enter to
        // re-attach" but it silently does nothing. (§4.2.2 / §11.5)
        if let Some(entry) = self.live.get(session_id) {
            if entry.status == SessionStatus::Exited {
                self.live.remove(session_id);
            } else {
                return Ok(session_id.to_string());
            }
        }
        check_occupiable(session_id)?;
        let lock = SessionLock::try_acquire(session_id)?;
        if !self.provider.omp_available() {
            bail!(
                "omp not found at '{}'. Install omp and ensure it is on PATH.",
                self.provider.omp_bin
            );
        }
        let args = self.provider.spawn_resume_args(cwd, session_id);
        let pins = self.config.effective_pi_pins();
        let pty = PtySession::spawn(&self.provider.omp_bin, &args, cwd, rows, cols, &pins, self.kitty_keyboard)
            .with_context(|| format!("spawn omp --resume {session_id}"))?;
        self.live.insert(
            session_id.to_string(),
            LiveEntry {
                pty,
                lock: Some(lock),
                status: SessionStatus::Starting,
                title: title.to_string(),
                title_kind,
                cwd: cwd.to_path_buf(),
                workspace_id: workspace_id.to_string(),
                spawned_at: Utc::now(),
            },
        );
        Ok(session_id.to_string())
    }

    /// Spawn a brand-new omp session.
    pub fn attach_new(
        &mut self,
        workspace_id: &str,
        cwd: &Path,
        rows: u16,
        cols: u16,
    ) -> Result<String> {
        if !self.provider.omp_available() {
            bail!(
                "omp not found at '{}'. Install omp and ensure it is on PATH.",
                self.provider.omp_bin
            );
        }
        let id = format!("new-{}", self.next_new);
        self.next_new += 1;
        let lock = SessionLock::try_acquire(&id)?;
        let args = self.provider.spawn_new_args(cwd);
        let pins = self.config.effective_pi_pins();
        let pty = PtySession::spawn(&self.provider.omp_bin, &args, cwd, rows, cols, &pins, self.kitty_keyboard)
            .context("spawn omp")?;
        self.live.insert(
            id.clone(),
            LiveEntry {
                pty,
                lock: Some(lock),
                status: SessionStatus::Starting,
                title: "New session".into(),
                title_kind: TitleKind::Fallback,
                cwd: cwd.to_path_buf(),
                workspace_id: workspace_id.to_string(),
                spawned_at: Utc::now(),
            },
        );
        Ok(id)
    }

    pub fn close_session(&mut self, id: &str) {
        if let Some(mut entry) = self.live.remove(id) {
            // Hold the flock until the kill ladder finishes so a
            // close→reattach race can't attach a new `omp --resume` while
            // the old child is still dying and writing the session file.
            // (§2 occupied / §4.2.11.5)
            let handle = std::thread::spawn(move || {
                entry.pty.kill_process_group();
                drop(entry.lock.take());
            });
            self.kill_threads.push(handle);
        }
    }

    /// Kill a live session and wait for the SIGHUP→SIGTERM→SIGKILL ladder
    /// (and flock release) before returning. Use before deleting its jsonl so
    /// a dying omp cannot recreate the file under us.
    pub fn close_session_blocking(&mut self, id: &str) {
        if let Some(mut entry) = self.live.remove(id) {
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

    /// Live session ids belonging to `workspace_id` (non-exited).
    pub fn live_ids_for_workspace(&self, workspace_id: &str) -> Vec<String> {
        self.live
            .iter()
            .filter(|(_, e)| e.workspace_id == workspace_id && !e.pty.is_exited())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Close every live session for a workspace. Returns how many were closed.
    pub fn close_workspace_sessions(&mut self, workspace_id: &str) -> usize {
        let ids = self.live_ids_for_workspace(workspace_id);
        let n = ids.len();
        for id in ids {
            self.close_session(&id);
        }
        self.known_disk_ids.remove(workspace_id);
        n
    }

    /// Drain host-bound escapes (OSC 52 etc.) for every live session.
    ///
    /// Only bytes from the `focused` session reach the outer terminal;
    /// bytes from background sessions are discarded to keep their
    /// `host_outbound` bounded (a background child emitting OSC 52
    /// ClipboardStore would otherwise grow it without bound). (§4.2.11.2)
    pub fn drain_host_outbound(&mut self, focused: Option<&str>) -> Vec<u8> {
        let mut out = Vec::new();
        for (id, entry) in &mut self.live {
            let host = entry.pty.take_host_outbound();
            if host.is_empty() {
                continue;
            }
            if Some(id.as_str()) == focused {
                out.extend_from_slice(&host);
            }
            // Background sessions' host-bound bytes are dropped: a
            // non-focused child must not mutate the outer clipboard.
        }
        out
    }

    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        for entry in self.live.values_mut() {
            let _ = entry.pty.resize(rows, cols);
        }
    }

    /// Poll child liveness. Returns session ids that **just** transitioned to
    /// exited/error (lock released this call) so the UI can drop to Nav.
    pub fn poll_exits(&mut self) -> Vec<String> {
        let mut just_exited = Vec::new();
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            let Some(entry) = self.live.get_mut(&id) else {
                continue;
            };
            // Transition Starting → Running once VT reports readiness.
            // (§5.2 status enum — Starting is the spawn-issued-but-not-ready gap.)
            if entry.status == SessionStatus::Starting && entry.pty.is_ready() {
                entry.status = SessionStatus::Running;
            }
            if entry.pty.is_exited() && entry.lock.is_some() {
                // Distinguish clean exit (success) from error (non-zero / signal).
                // (§8 — child exits unexpectedly → mark exited/error.)
                let success = entry.pty.exit_success();
                entry.status = match success {
                    Some(true) => SessionStatus::Exited,
                    _ => SessionStatus::Error,
                };
                // Child is gone — release the flock so future attaches aren't blocked.
                entry.lock = None;
                just_exited.push(id);
            }
        }
        just_exited
    }


    /// Synchronous teardown for final exit — blocks on the graceful
    /// SIGHUP→SIGTERM→SIGKILL ladder per child so processes aren't
    pub fn shutdown_all_blocking(&mut self) {
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            if let Some(mut entry) = self.live.remove(&id) {
                // Kill first, then release the flock — mirrors close_session's
                // ordering so a close→reattach race can't attach a new omp --resume
                // while the old child is still dying and writing the session file.
                // (§2 occupied / §4.2.11.5)
                entry.pty.kill_process_group();
                drop(entry.lock.take());
            }
        }
        // Join detached kill threads from close_session so the full
        // SIGHUP→SIGTERM→SIGKILL ladder completes before amux exits.
        // Without this, process exit can abort a mid-ladder thread and
        // orphan a child that ignores SIGHUP. (§4.2.8 / E14)
        for handle in std::mem::take(&mut self.kill_threads) {
            let _ = handle.join();
        }
    }
}

impl Drop for SessionSupervisor {
    fn drop(&mut self) {
        self.shutdown_all_blocking();
    }
}
