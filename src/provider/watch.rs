//! Watch an omp session directory for jsonl changes (title / new sessions).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum SessionDirEvent {
    /// A `.jsonl` was created, modified, or renamed into place.
    Changed(PathBuf),
    /// A `.jsonl` was removed.
    Removed(PathBuf),
    /// Watcher overflow / error — caller should full-rescan.
    Rescan,
}

/// Non-blocking session-dir watcher. Events are coalesced by the UI layer.
pub struct SessionDirWatcher {
    rx: Receiver<SessionDirEvent>,
    cmd_tx: Sender<WatchCmd>,
    join: Option<JoinHandle<()>>,
    watched: Option<PathBuf>,
}

enum WatchCmd {
    SetPath(Option<PathBuf>),
    Shutdown,
}

impl SessionDirWatcher {
    pub fn spawn() -> Self {
        let (ev_tx, rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("amux-session-watch".into())
            .spawn(move || watch_thread(cmd_rx, ev_tx))
            .ok();
        Self {
            rx,
            cmd_tx,
            join,
            watched: None,
        }
    }

    pub fn watched_path(&self) -> Option<&Path> {
        self.watched.as_deref()
    }

    /// Watch `dir` (non-recursive). Pass `None` to stop watching.
    ///
    /// Re-sends when the same path is requested again so a previously missing
    /// directory can be picked up once omp creates it.
    pub fn set_dir(&mut self, dir: Option<PathBuf>) {
        let same = self.watched == dir;
        self.watched = dir.clone();
        if same {
            if let Some(ref d) = dir {
                if !d.is_dir() {
                    return;
                }
            } else {
                return;
            }
        }
        let _ = self.cmd_tx.send(WatchCmd::SetPath(dir));
    }

    /// Drain pending events without blocking.
    pub fn drain(&self) -> Vec<SessionDirEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

impl Drop for SessionDirWatcher {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WatchCmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn watch_thread(cmd_rx: Receiver<WatchCmd>, ev_tx: Sender<SessionDirEvent>) {
    let current: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let current_for_cb = Arc::clone(&current);
    let ev_tx_cb = ev_tx.clone();

    let mut watcher = match notify::recommended_watcher(
        move |res: Result<notify::Event, notify::Error>| match res {
            Ok(event) => forward_event(&ev_tx_cb, &current_for_cb, event),
            Err(e) => {
                tracing::warn!(target: "amux", "session dir watch error: {e}");
                let _ = ev_tx_cb.send(SessionDirEvent::Rescan);
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(target: "amux", "session dir watcher unavailable: {e}");
            // Still process SetPath/Shutdown so UI can fall back to polling.
            loop {
                match cmd_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(WatchCmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(WatchCmd::SetPath(_)) => {}
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            return;
        }
    };

    loop {
        match cmd_rx.recv() {
            Ok(WatchCmd::Shutdown) | Err(_) => break,
            Ok(WatchCmd::SetPath(next)) => {
                if let Some(prev) = current.lock().take() {
                    let _ = watcher.unwatch(&prev);
                }
                if let Some(ref dir) = next {
                    if !dir.exists() {
                        // Parent may exist; try create watch later via Rescan from UI poll.
                        *current.lock() = None;
                        continue;
                    }
                    match watcher.watch(dir, RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            *current.lock() = Some(dir.clone());
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "amux",
                                "watch {}: {e}",
                                dir.display()
                            );
                            let _ = ev_tx.send(SessionDirEvent::Rescan);
                        }
                    }
                }
            }
        }
    }
}

fn forward_event(
    ev_tx: &Sender<SessionDirEvent>,
    current: &Arc<Mutex<Option<PathBuf>>>,
    event: notify::Event,
) {
    let Some(watched) = current.lock().clone() else {
        return;
    };
    let paths: Vec<PathBuf> = event
        .paths
        .into_iter()
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && p.parent().is_some_and(|parent| parent == watched)
        })
        .collect();
    if paths.is_empty() {
        // New files appearing under the dir (e.g. first .jsonl) — rescan.
        if matches!(event.kind, EventKind::Create(_)) {
            let _ = ev_tx.send(SessionDirEvent::Rescan);
        }
        return;
    }
    match event.kind {
        EventKind::Remove(_) => {
            for p in paths {
                let _ = ev_tx.send(SessionDirEvent::Removed(p));
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any => {
            for p in paths {
                let _ = ev_tx.send(SessionDirEvent::Changed(p));
            }
        }
        EventKind::Other => {
            let _ = ev_tx.send(SessionDirEvent::Rescan);
        }
        _ => {}
    }
}
