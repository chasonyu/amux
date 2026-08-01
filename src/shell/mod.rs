//! Dual-pane shell: sidebar + PTY surface, focus, modals, key routing.

pub mod dir_browser;
pub mod mode_mirror;
mod text_input;

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use chrono::{DateTime, Utc};


use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use signal_hook::consts::signal::{SIGHUP, SIGWINCH};
use signal_hook::flag as signal_flag;

use crate::appearance::{probe_appearance, Appearance};
use crate::config::AmuxConfig;
use crate::theme::Theme;
use crate::escape::{EscapeAction, EscapeToggle};
use crate::mouse::{
    list_row_index, point_in_rect, sgr_has_shift, sgr_is_button_press, sgr_is_release,
    translate_sgr_mouse_clipped,
};
use crate::provider::{
    delete_session_with_artifacts, load, refresh_disk_session, render_blocks, RenderedLine,
    SessionDirEvent, SessionDirWatcher, SpanStyle, TitleKind, TranscriptBlock, TranscriptRole,
};
use crate::pty::MirroredModes;
use crate::raw_input::{is_sgr_mouse, RawInputParser};
use crate::session::{SessionStatus, SessionSummary, SessionSupervisor};
use crate::workspace::WorkspaceStore;

const TITLE_WATCH_DEBOUNCE: Duration = Duration::from_millis(80);
const TITLE_FALLBACK_POLL: Duration = Duration::from_secs(3);

use self::dir_browser::{draw_dir_browser, BrowserResult, DirBrowser};
use self::mode_mirror::{apply_host_modes, apply_nav_host_modes, baseline_host_modes, KbNegotiated};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Nav,
    Agent,
    Modal,
}

#[derive(Debug)]
enum Modal {
    DirBrowser(DirBrowser),
    Help,
    ConfirmQuit {
        /// Which button has keyboard focus (`true` = Yes).
        yes_focused: bool,
    },
    /// Remove workspace from `workspaces.json` (omp session files kept).
    ConfirmRemoveWorkspace {
        id: String,
        name: String,
        live: usize,
        yes_focused: bool,
    },
    /// Delete omp session jsonl + sibling artifacts dir.
    ConfirmDeleteSession {
        id: String,
        title: String,
        path: PathBuf,
        live: bool,
        yes_focused: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmResult {
    Yes,
    No,
    /// Modal stays open (e.g. moved focus between buttons).
    Keep,
}

pub struct App {
    #[allow(dead_code)]
    config: AmuxConfig,
    workspaces: WorkspaceStore,
    sessions: SessionSupervisor,
    focus: Focus,
    selected_ws: usize,
    selected_session: usize,
    session_list: Vec<SessionSummary>,
    focused_session_id: Option<String>,
    modal: Option<Modal>,
    status: String,
    drop_notice: Option<String>,
    total_dropped_keys: u64,
    total_write_drops: u64,
    escape: EscapeToggle,
    parser: RawInputParser,
    last_host_modes: MirroredModes,
    kb: KbNegotiated,
    sidebar_width: u16,
    /// Left pane outer rect (includes Borders::ALL); empty when collapsed.
    sidebar_rect: Rect,
    /// Workspace list cells (Length(7) chunk); row `i` at `y + i`.
    ws_list_rect: Rect,
    /// Sessions block TOP-inner (not chunks[1]); row `i` at `y + i`.
    sess_list_rect: Rect,
    /// Agent PTY / preview **inner** content area.
    pty_area: Rect,
    should_quit: bool,
    winch: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
    esc_deadline: Option<Instant>,
    startup_hint_at: Option<Instant>,
    /// Sidebar auto-collapsed due to narrow terminal (hint in status). (§4.2.7.6)
    sidebar_collapsed: bool,
    /// A mouse button is currently held (drag active) — gates the
    /// leave-AgentMode button-release. (§4.2.2)
    mouse_button_down: bool,
    /// Last translated mouse position (1-based, pane-local) and button
    /// (Cb low bits) for a faithful leave-AgentMode release. (§4.2.2)
    last_mouse_cb: u8,
    last_mouse_x: u16,
    last_mouse_y: u16,
    theme: Theme,
    /// Host terminal appearance from OSC 11 (startup probe; Mode 2031 later).
    #[allow(dead_code)]
    appearance: Appearance,
    /// Watches current workspace omp session dir for title / list changes.
    session_watch: SessionDirWatcher,
    dirty_session_paths: HashSet<PathBuf>,
    title_debounce_until: Option<Instant>,
    title_need_rescan: bool,
    last_title_poll: Instant,
    /// Cached JSONL transcript for the Agent preview pane.
    transcript_cache: Option<TranscriptCache>,
}

struct TranscriptCache {
    path: PathBuf,
    mtime: DateTime<Utc>,
    size: u64,
    blocks: Vec<TranscriptBlock>,
}

impl App {
    pub fn new() -> Result<Self> {
        AmuxConfig::ensure_dirs()?;
        let config = AmuxConfig::load().unwrap_or_default();
        let workspaces = WorkspaceStore::load()?;
        let kb = KbNegotiated::probe();
        let sessions = SessionSupervisor::new(config.clone(), kb.kitty);
        let escape = EscapeToggle::new(config.escape_byte());
        Ok(Self {
            config,
            workspaces,
            sessions,
            focus: Focus::Nav,
            selected_ws: 0,
            selected_session: 0,
            session_list: Vec::new(),
            focused_session_id: None,
            modal: None,
            status: String::new(),
            // Overwritten in `run` after OSC 11 probe (before raw/alt screen).
            theme: Theme::default(),
            appearance: Appearance::Dark,
            total_dropped_keys: 0,
            total_write_drops: 0,
            drop_notice: None,
            escape,
            parser: RawInputParser::default(),
            last_host_modes: MirroredModes::default(),
            kb,
            sidebar_width: 32,
            sidebar_rect: Rect::default(),
            ws_list_rect: Rect::default(),
            sess_list_rect: Rect::default(),
            pty_area: Rect::default(),
            winch: Arc::new(AtomicBool::new(false)),
            should_quit: false,
            interrupt: Arc::new(AtomicBool::new(false)),
            esc_deadline: None,
            startup_hint_at: None,
            sidebar_collapsed: false,
            mouse_button_down: false,
            last_mouse_cb: 0,
            last_mouse_x: 1,
            last_mouse_y: 1,
            session_watch: SessionDirWatcher::spawn(),
            dirty_session_paths: HashSet::new(),
            title_debounce_until: None,
            title_need_rescan: false,
            last_title_poll: Instant::now(),
            transcript_cache: None,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let mut stdout = io::stdout();

        // OSC 11 appearance probe timing:
        // - Prefer **before** `enable_raw_mode` so stdin is still cooked and
        //   the ~200ms poll reply isn't mixed with raw-mode framing.
        // - Prefer **before** `EnterAlternateScreen` (primary screen) when
        //   possible; if already in alt screen, querying once still works.
        // - Fail / timeout → Dark (handled inside `probe_appearance`).
        {
            let mut stdin = io::stdin();
            let appearance = probe_appearance(&mut stdout, &mut stdin);
            self.appearance = appearance;
            self.theme = Theme::for_appearance(appearance);
            tracing::info!(
                target: "amux",
                "startup: appearance={appearance:?}"
            );
        }

        enable_raw_mode().context("enable_raw_mode")?;

        // Setup + event loop. Any `?` inside run_inner returns Err here;
        // the teardown below ALWAYS runs — even on setup failure. (§4.2.10)
        let stdin_fd = io::stdin().as_raw_fd();
        let result = self.run_inner(&mut stdout, stdin_fd);

        // Teardown — restores terminal on every exit path. (§4.2.10 / E14)
        self.sessions.shutdown_all_blocking();
        let _ = set_nonblocking(stdin_fd, false);
        let _ = baseline_host_modes(&mut io::stdout());
        if self.kb.kitty {
            let _ = write!(io::stdout(), "\x1b[<u");
        }
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        result
    }

    fn run_inner(
        &mut self,
        stdout: &mut io::Stdout,
        stdin_fd: i32,
    ) -> Result<()> {
        execute!(
            stdout,
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        // Also pin click+SGR via raw CSI (keeps Nav baseline if crossterm
        // EnableMouseCapture set differs across versions).
        apply_nav_host_modes(stdout)?;
        // Soft keyboard probe enable if host supports
        if self.kb.kitty {
            let _ = write!(stdout, "\x1b[>3u"); // disambiguate + report-alternate-keys
            let _ = stdout.flush();
        }

        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        signal_flag::register(SIGWINCH, Arc::clone(&self.winch))?;
        // Restore terminal on SIGINT/SIGTERM — never let signals bypass teardown.
        signal_flag::register(signal_hook::consts::signal::SIGINT, Arc::clone(&self.interrupt))?;
        signal_flag::register(signal_hook::consts::signal::SIGTERM, Arc::clone(&self.interrupt))?;
        // SIGHUP (controlling terminal closed) must break into the normal
        // teardown path so the kill ladder runs and the terminal is restored
        // on every exit path. (§4.2.10)
        signal_flag::register(SIGHUP, Arc::clone(&self.interrupt))?;

        self.refresh_sessions();
        // Log the negotiated keyboard level and the escape intercept so the
        // embedding contract is observable. (§4.2.3a.5 / §4.2.4 MUST)
        tracing::info!(
            target: "amux",
            "startup: kb={} kitty={} modify_other_keys={} escape=0x{:02x} double_tap=500ms",
            self.kb.label(),
            self.kb.kitty,
            self.kb.modify_other_keys,
            self.escape.escape_byte(),
        );

        // Do NOT set O_NONBLOCK on stdin. On typical TTYs fds 0/1/2 share the
        // same open-file description, so O_NONBLOCK would also make stdout
        // non-blocking — ratatui/crossterm writes then fail with EAGAIN
        // ("Resource temporarily unavailable"). Use poll() for readiness, then
        // a normal blocking read (returns immediately when POLLIN).
        let _ = set_nonblocking(stdin_fd, false);

        self.event_loop(&mut terminal, stdin_fd)
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        stdin_fd: i32,
    ) -> Result<()> {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 4096];

        while !self.should_quit {
            if self.interrupt.swap(false, Ordering::SeqCst) {
                break;
            }
            if self.winch.swap(false, Ordering::SeqCst) {
                let size = terminal.size()?;
                terminal.resize(ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: size.width,
                    height: size.height,
                })?;
                // Recompute pty_area from the NEW terminal size so all live
                // PTYs get the correct dimensions, not the stale pre-resize
                // value. (§4.2.7.2)
                let area = compute_pty_area(ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: size.width,
                    height: size.height,
                });
                self.pty_area = area;
                if area.width > 0 && area.height > 0 {
                    self.sessions.resize_all(area.height, area.width);
                }
            }

            // Escape double-tap / bare Esc deadlines
            let now = Instant::now();
            if let Some(action) = self.escape.poll(now) {
                self.apply_escape_action(action)?;
            }
            if self.esc_deadline.map(|d| now >= d).unwrap_or(false) {
                self.esc_deadline = None;
                // Bare Esc ambiguity timeout (same 25ms as dux). Resolve without
                // re-feeding the parser — feed_sequences would hang Esc again.
                if let Some(bytes) = self.parser.resolve_pending_esc() {
                    match self.focus {
                        Focus::Agent => self.route_agent_bytes(&bytes)?,
                        // Shell/Modal: deliver Esc to chrome (close modal, etc.).
                        Focus::Nav | Focus::Modal => {
                            if self.modal.is_some() {
                                self.handle_modal_seq(&bytes)?;
                            } else {
                                self.handle_shell_seq(&bytes)?;
                            }
                        }
                    }
                }
            }

            let just_exited = self.sessions.poll_exits();
            if self.focus == Focus::Agent {
                if let Some(id) = self.focused_session_id.clone() {
                    if just_exited.iter().any(|e| e == &id) {
                        // Spec: child exit → drop to Nav so x / Enter work.
                        self.enter_nav()?;
                        self.status =
                            "Session exited — Enter re-attach · x close".into();
                        self.refresh_sessions();
                    }
                }
            }
            self.check_startup_drops();
            self.poll_session_titles(now);

            // Mode mirror for focused session
            if self.focus == Focus::Agent {
                if let Some(id) = self.focused_session_id.clone() {
                    if let Some(pty) = self.sessions.get(&id) {
                        let modes = pty.mirrored_modes();
                        if modes != self.last_host_modes {
                            apply_host_modes(&mut io::stdout(), &modes)?;
                            self.last_host_modes = modes;
                        }
                    }
                }
            }
            // Drain host-bound escapes (OSC 52 etc.) for every live session;
            // only the focused session's bytes reach the outer terminal, the
            // rest are dropped to keep background host_outbound bounded.
            // (§4.2.11.2 / PTY-15)
            let focused = if self.focus == Focus::Agent {
                self.focused_session_id.as_deref()
            } else {
                None
            };
            let host = self.sessions.drain_host_outbound(focused);
            if !host.is_empty() {
                let mut out = io::stdout();
                out.write_all(&host)?;
                out.flush()?;
            }

            terminal.draw(|f| self.draw(f))?;
            // Backpressure: stop ingesting stdin while the focused PTY write
            // queue is near-full so dropped keystrokes stay counted, not
            // silently multiplied. (§4.2.8)
            if self.focus == Focus::Agent {
                let bp = self
                    .focused_session_id
                    .as_ref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|pty| pty.is_write_backpressured())
                    .unwrap_or(false);
                if bp {
                    continue;
                }
            }


            // Poll stdin with short timeout so UI stays responsive
            let timeout = self.next_timeout();
            let readable = poll_fd(stdin_fd, timeout)?;
            if !readable {
                continue;
            }

            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = &buf[..n];
                    match self.focus {
                        Focus::Agent => self.handle_agent_raw(bytes)?,
                        Focus::Nav | Focus::Modal => self.handle_shell_bytes(bytes)?,
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn next_timeout(&self) -> Duration {
        let mut t = Duration::from_millis(33);
        if let Some(d) = self.escape.deadline() {
            t = t.min(d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)));
        }
        if let Some(d) = self.esc_deadline {
            t = t.min(d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)));
        }
        if let Some(d) = self.title_debounce_until {
            t = t.min(d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)));
        }
        t
    }

    fn ensure_session_watch(&mut self) {
        let dir = self
            .workspaces
            .list()
            .get(self.selected_ws)
            .map(|ws| {
                self.sessions
                    .provider()
                    .session_dir_for_cwd(Path::new(&ws.path))
            });
        self.session_watch.set_dir(dir);
    }

    fn poll_session_titles(&mut self, now: Instant) {
        for ev in self.session_watch.drain() {
            match ev {
                SessionDirEvent::Changed(p) | SessionDirEvent::Removed(p) => {
                    self.dirty_session_paths.insert(p);
                    self.title_debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
                }
                SessionDirEvent::Rescan => {
                    self.title_need_rescan = true;
                    self.title_debounce_until = Some(now + TITLE_WATCH_DEBOUNCE);
                }
            }
        }

        let debounce_due = self
            .title_debounce_until
            .map(|d| now >= d)
            .unwrap_or(false);
        if debounce_due {
            self.title_debounce_until = None;
            if self.title_need_rescan {
                self.title_need_rescan = false;
                self.dirty_session_paths.clear();
                self.refresh_sessions();
            } else if !self.dirty_session_paths.is_empty() {
                let paths: Vec<PathBuf> = self.dirty_session_paths.drain().collect();
                self.apply_session_file_changes(&paths);
            }
        }

        if now.duration_since(self.last_title_poll) >= TITLE_FALLBACK_POLL {
            self.last_title_poll = now;
            self.fallback_poll_titles();
        }
    }

    fn apply_session_file_changes(&mut self, paths: &[PathBuf]) {
        let mut need_full = false;
        for path in paths {
            if !path.exists() {
                need_full = true;
                break;
            }
            let known = self
                .session_list
                .iter()
                .any(|s| s.path.as_ref().is_some_and(|p| p == path));
            if !known {
                need_full = true;
                break;
            }
        }
        if need_full {
            self.refresh_sessions();
            return;
        }
        for path in paths {
            let Some(disk) = refresh_disk_session(path) else {
                continue;
            };
            self.sessions.apply_disk_title(&disk);
            if let Some(s) = self.session_list.iter_mut().find(|s| s.id == disk.id) {
                s.title = disk.title.clone();
                s.title_kind = disk.title_kind;
                s.is_fork = disk.parent_session.is_some();
                s.mtime = disk.mtime;
                s.size = disk.size;
                s.path = Some(disk.path);
            }
        }
    }

    fn fallback_poll_titles(&mut self) {
        let mut changed = false;
        let mut need_full = false;
        let snapshots: Vec<(String, Option<PathBuf>, DateTime<Utc>, u64)> = self
            .session_list
            .iter()
            .map(|s| (s.id.clone(), s.path.clone(), s.mtime, s.size))
            .collect();

        for (id, path, mtime, size) in snapshots {
            let Some(path) = path else {
                // Synthetic live entry — full refresh may adopt uuid + title.
                need_full = true;
                break;
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                need_full = true;
                break;
            };
            let new_mtime = DateTime::<Utc>::from(meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH));
            let new_size = meta.len();
            if new_mtime == mtime && new_size == size {
                continue;
            }
            let Some(disk) = refresh_disk_session(&path) else {
                need_full = true;
                break;
            };
            if disk.id != id {
                need_full = true;
                break;
            }
            self.sessions.apply_disk_title(&disk);
            if let Some(s) = self.session_list.iter_mut().find(|s| s.id == id) {
                let is_fork = disk.parent_session.is_some();
                if s.title != disk.title
                    || s.title_kind != disk.title_kind
                    || s.is_fork != is_fork
                {
                    changed = true;
                }
                s.title = disk.title;
                s.title_kind = disk.title_kind;
                s.is_fork = is_fork;
                s.mtime = disk.mtime;
                s.size = disk.size;
            }
        }

        // Also detect brand-new jsonl files not yet in the list.
        if !need_full {
            if let Some(ws) = self.workspaces.list().get(self.selected_ws) {
                if let Ok(disk) = self
                    .sessions
                    .provider()
                    .list_sessions(Path::new(&ws.path))
                {
                    if disk.len()
                        != self
                            .session_list
                            .iter()
                            .filter(|s| s.path.is_some())
                            .count()
                    {
                        need_full = true;
                    }
                }
            }
        }

        if need_full {
            self.refresh_sessions();
        } else if changed {
            // titles updated in place
        }
    }

    fn handle_agent_raw(&mut self, bytes: &[u8]) -> Result<()> {
        let seqs = self.parser.feed_sequences(bytes);
        if self.parser.pending_is_bare_esc() {
            self.esc_deadline = Some(Instant::now() + Duration::from_millis(25));
        }
        for seq in seqs {
            if seq.in_bracket_paste
                || seq.bytes == crate::raw_input::BRACKET_PASTE_START
                || seq.bytes == crate::raw_input::BRACKET_PASTE_END
            {
                // No intercepts inside paste — cancel pending escape without
                // toggling, then forward markers+payload verbatim. (§4.2.3 #4)
                self.escape.clear();
                self.route_agent_bytes(&seq.bytes)?;
                continue;
            }

            if self.escape.is_escape_seq(&seq.bytes) {
                let action = self.escape.on_escape(Instant::now());
                match action {
                    EscapeAction::Armed => {
                        // wait for double-tap or timeout
                    }
                    EscapeAction::ToggleNav => self.enter_nav()?,
                    EscapeAction::ForwardLiteral => {
                        self.route_agent_bytes(&[self.escape.escape_byte()])?;
                    }
                }
                continue;
            }

            if let Some(a) = self.escape.on_other_input() {
                self.apply_escape_action(a)?;
                // ToggleNav may have fired — route the current completed
                // sequence to Nav (its new destination) and stop
                // agent processing so no later bytes in this chunk reach
                // the child PTY while in Nav.
                if self.focus != Focus::Agent {
                    self.handle_shell_bytes(&seq.bytes)?;
                    return Ok(());
                }
            }

            // Mouse: shift yield; in-pane → forward only if child wants mouse;
            // sidebar press → Nav/select. Host always keeps 1000+1006 so chrome
            // clicks work even when omp has not enabled mouse.
            if is_sgr_mouse(&seq.bytes) {
                if sgr_has_shift(&seq.bytes) {
                    continue; // outer terminal selection
                }
                let area = self.pty_area;
                let child_wants_mouse = self
                    .focused_session_id
                    .as_ref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|pty| {
                        let m = pty.mirrored_modes();
                        m.mouse_1000 || m.mouse_1002 || m.mouse_1003
                    })
                    .unwrap_or(false);
                if let Some(translated) = translate_sgr_mouse_clipped(
                    &seq.bytes,
                    area.x,
                    area.y,
                    area.width,
                    area.height,
                ) {
                    if child_wants_mouse {
                        // Track button-down only when forwarding in-pane (B2).
                        if sgr_is_release(&seq.bytes) {
                            self.mouse_button_down = false;
                        } else if sgr_is_button_press(&seq.bytes) {
                            self.mouse_button_down = true;
                        }
                        if let Some((cb, x, y)) = parse_sgr_cxy(&translated) {
                            self.last_mouse_cb = cb;
                            self.last_mouse_x = x;
                            self.last_mouse_y = y;
                        }
                        self.route_agent_bytes(&translated)?;
                    }
                    // else: child ignored mouse — swallow in-pane events (already Agent)
                } else if sgr_is_button_press(&seq.bytes) {
                    // Outside PTY inner: sidebar press → Nav/select only.
                    // Do not set mouse_button_down (no fake release on enter_nav).
                    if let Some((x, y)) = parse_sgr_xy(&seq.bytes) {
                        let side = self.sidebar_rect;
                        if self.sidebar_width > 0
                            && point_in_rect(x, y, side.x, side.y, side.width, side.height)
                        {
                            self.dispatch_shell_hit(x, y)?;
                        }
                    }
                }
                continue;
            }

            self.route_agent_bytes(&seq.bytes)?;
        }
        Ok(())
    }

    fn route_agent_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(id) = self.focused_session_id.clone() else {
            return Ok(());
        };
        if let Some(pty) = self.sessions.get(&id) {
            match pty.enqueue_write(bytes) {
                Ok(()) => {}
                Err(e) => self.status = format!("write: {e}"),
            }
        }
        Ok(())
    }

    fn apply_escape_action(&mut self, action: EscapeAction) -> Result<()> {
        match action {
            EscapeAction::Armed => {}
            EscapeAction::ToggleNav => self.enter_nav()?,
            EscapeAction::ForwardLiteral => {
                self.route_agent_bytes(&[self.escape.escape_byte()])?;
            }
        }
        Ok(())
    }

    fn enter_nav(&mut self) -> Result<()> {
        if self.focus == Focus::Agent {
            if let Some(id) = self.focused_session_id.clone() {
                if let Some(pty) = self.sessions.get(&id) {
                    let modes = pty.mirrored_modes();
                    if modes.focus {
                        let _ = pty.enqueue_write_forced(b"\x1b[O");
                    }
                    // Send a mouse button-release only when a button is
                    // actually held (drag active) and the child has a mouse
                    // mode enabled. A release with no button down is a
                    // spurious event. (§4.2.2)
                    if (modes.mouse_1000 || modes.mouse_1002 || modes.mouse_1003)
                        && self.mouse_button_down
                    {
                        let cb = self.last_mouse_cb & 0x03;
                        let x = self.last_mouse_x.max(1);
                        let y = self.last_mouse_y.max(1);
                        let _ = pty
                            .enqueue_write_forced(format!("\x1b[<{cb};{x};{y}m").as_bytes());
                    }
                    self.mouse_button_down = false;
                }
            }
        }
        self.focus = Focus::Nav;
        self.escape.clear();
        // Re-arm full mouse stack (dux does the same after clear / on UI mode).
        execute!(io::stdout(), EnableMouseCapture)?;
        apply_nav_host_modes(&mut io::stdout())?;
        self.last_host_modes = MirroredModes::default();
        // Mode/kb/session live in the powerline status — don't repeat shortcuts.
        self.status.clear();
        Ok(())
    }

    fn enter_agent(&mut self) -> Result<()> {
        let Some(id) = self.focused_session_id.clone() else {
            self.status = "No session focused".into();
            return Ok(());
        };
        // Ensure size
        let area = self.pty_area;
        if area.width > 0 && area.height > 0 {
            if let Some(pty) = self.sessions.get(&id) {
                let _ = pty.resize(area.height, area.width);
            }
        }
        self.focus = Focus::Agent;
        if let Some(pty) = self.sessions.get(&id) {
            let modes = pty.mirrored_modes();
            apply_host_modes(&mut io::stdout(), &modes)?;
            self.last_host_modes = modes;
            if modes.focus {
                let _ = pty.enqueue_write_forced(b"\x1b[I");
            }
            self.startup_hint_at = if pty.is_ready() {
                None
            } else {
                Some(Instant::now())
            };
        }
        self.status.clear();
        Ok(())
    }

    fn handle_shell_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        // In Shell/Modal we use a simple key decode for navigation.
        // Still parse sequences so CSI arrows work.
        let seqs = self.parser.feed_sequences(bytes);
        // Arm bare-Esc ambiguity timeout (Agent path does the same). Without
        // this, pending `\x1b` sits forever and the next key becomes Alt+key.
        if self.parser.pending_is_bare_esc() {
            self.esc_deadline = Some(Instant::now() + Duration::from_millis(25));
        }
        for seq in seqs {
            // Bracketed paste must not fire Nav shortcuts (q/n/j/Enter/…).
            // Markers + payload are dropped; Agent path forwards instead.
            if seq.in_bracket_paste
                || seq.bytes == crate::raw_input::BRACKET_PASTE_START
                || seq.bytes == crate::raw_input::BRACKET_PASTE_END
            {
                self.escape.clear();
                continue;
            }
            if is_sgr_mouse(&seq.bytes) {
                if sgr_has_shift(&seq.bytes) {
                    continue; // outer terminal selection
                }
                // Modal: ignore hit-test clicks (keys still handle modal).
                if self.modal.is_some() {
                    continue;
                }
                // Nav: shell hit-test on button press only (B1).
                if sgr_is_button_press(&seq.bytes) {
                    if let Some((x, y)) = parse_sgr_xy(&seq.bytes) {
                        self.dispatch_shell_hit(x, y)?;
                    }
                }
                continue;
            }
            if self.escape.is_escape_seq(&seq.bytes) && self.modal.is_none() {
                // Toggle to agent if we have a focused session
                if self.focused_session_id.is_some() {
                    self.enter_agent()?;
                }
                continue;
            }
            if self.modal.is_some() {
                self.handle_modal_seq(&seq.bytes)?;
            } else {
                self.handle_shell_seq(&seq.bytes)?;
            }
        }
        Ok(())
    }

    /// Press-only hit-test: Agent inner → attach; sidebar → Nav + select/chrome.
    fn dispatch_shell_hit(&mut self, x: u16, y: u16) -> Result<()> {
        let pty = self.pty_area;
        if point_in_rect(x, y, pty.x, pty.y, pty.width, pty.height) {
            return self.attach_selected();
        }

        let side = self.sidebar_rect;
        if self.sidebar_width == 0
            || !point_in_rect(x, y, side.x, side.y, side.width, side.height)
        {
            return Ok(()); // status bar / outside layout
        }

        // Any sidebar hit: enter Nav first, then select rules.
        self.enter_nav()?;

        let ws = self.ws_list_rect;
        if point_in_rect(x, y, ws.x, ws.y, ws.width, ws.height) {
            if let Some(i) = list_row_index(y, ws.y, ws.height, self.workspaces.list().len()) {
                self.selected_ws = i;
                self.selected_session = 0;
                self.refresh_sessions();
            }
            // else: empty rows in Length(7) → chrome (already Nav)
            return Ok(());
        }

        let sess = self.sess_list_rect;
        if point_in_rect(x, y, sess.x, sess.y, sess.width, sess.height) {
            if let Some(i) = list_row_index(y, sess.y, sess.height, self.session_list.len()) {
                self.selected_session = i;
            }
            // else: past last session → chrome
            return Ok(());
        }

        // Sidebar chrome (borders, Sessions TOP title, etc.)
        Ok(())
    }

    fn handle_shell_seq(&mut self, seq: &[u8]) -> Result<()> {
        match seq {
            b"q" | b"Q" => {
                self.modal = Some(Modal::ConfirmQuit { yes_focused: true });
                self.focus = Focus::Modal;
            }
            b"?" => {
                self.modal = Some(Modal::Help);
                self.focus = Focus::Modal;
            }
            b"a" | b"A" => self.open_dir_browser()?,
            b"n" | b"N" => self.new_session()?,
            b"j" | b"\x1b[B" => self.move_session(1),
            b"k" | b"\x1b[A" => self.move_session(-1),
            b"J" => self.move_workspace(1),
            b"K" => self.move_workspace(-1),
            b"D" => self.open_remove_workspace()?,
            b"d" => self.open_delete_session()?,
            b"\r" | b"\n" => self.attach_selected()?,
            b"x" => {
                if let Some(s) = self.session_list.get(self.selected_session) {
                    let id = s.id.clone();
                    self.sessions.close_session(&id);
                    if self.focused_session_id.as_deref() == Some(&id) {
                        self.focused_session_id = None;
                        self.enter_nav()?;
                    }
                    self.refresh_sessions();
                }
            }
            b"\x03" => {
                // Nav Ctrl+C: do not kill children
                self.status = "Ctrl+C ignored in Nav (use q to quit)".into();
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_modal_seq(&mut self, seq: &[u8]) -> Result<()> {
        let Some(modal) = self.modal.take() else {
            return Ok(());
        };
        match modal {
            Modal::Help => {
                self.focus = Focus::Nav;
            }
            Modal::ConfirmQuit { mut yes_focused } => {
                match confirm_key(seq, &mut yes_focused) {
                    ConfirmResult::Yes => self.should_quit = true,
                    ConfirmResult::No => self.focus = Focus::Nav,
                    ConfirmResult::Keep => {
                        self.modal = Some(Modal::ConfirmQuit { yes_focused });
                        self.focus = Focus::Modal;
                    }
                }
            }
            Modal::ConfirmRemoveWorkspace {
                id,
                name,
                live,
                mut yes_focused,
            } => match confirm_key(seq, &mut yes_focused) {
                ConfirmResult::Yes => self.confirm_remove_workspace(&id, &name)?,
                ConfirmResult::No => self.focus = Focus::Nav,
                ConfirmResult::Keep => {
                    self.modal = Some(Modal::ConfirmRemoveWorkspace {
                        id,
                        name,
                        live,
                        yes_focused,
                    });
                    self.focus = Focus::Modal;
                }
            },
            Modal::ConfirmDeleteSession {
                id,
                title,
                path,
                live,
                mut yes_focused,
            } => match confirm_key(seq, &mut yes_focused) {
                ConfirmResult::Yes => self.confirm_delete_session(&id, &title, &path)?,
                ConfirmResult::No => self.focus = Focus::Nav,
                ConfirmResult::Keep => {
                    self.modal = Some(Modal::ConfirmDeleteSession {
                        id,
                        title,
                        path,
                        live,
                        yes_focused,
                    });
                    self.focus = Focus::Modal;
                }
            },
            Modal::DirBrowser(browser) => match browser.handle_seq(seq) {
                BrowserResult::Close => {
                    self.focus = Focus::Nav;
                }
                BrowserResult::Continue(b) => {
                    self.modal = Some(Modal::DirBrowser(b));
                    self.focus = Focus::Modal;
                }
                BrowserResult::Add { path, browser } => {
                    match self.workspaces.add(&path) {
                        Ok(ws) => {
                            self.status = format!("Added workspace {}", ws.name);
                            self.selected_ws = self
                                .workspaces
                                .list()
                                .iter()
                                .position(|w| w.id == ws.id)
                                .unwrap_or(0);
                            self.refresh_sessions();
                            self.focus = Focus::Nav;
                        }
                        Err(e) => {
                            self.modal =
                                Some(Modal::DirBrowser(browser.with_error(e.to_string())));
                            self.focus = Focus::Modal;
                        }
                    }
                }
            },
        }
        Ok(())
    }

    fn open_dir_browser(&mut self) -> Result<()> {
        self.modal = Some(Modal::DirBrowser(DirBrowser::open()));
        self.focus = Focus::Modal;
        self.status.clear();
        Ok(())
    }

    fn open_remove_workspace(&mut self) -> Result<()> {
        let Some(ws) = self.workspaces.list().get(self.selected_ws).cloned() else {
            self.status = "No workspace to remove".into();
            return Ok(());
        };
        let live = self.sessions.live_ids_for_workspace(&ws.id).len();
        self.modal = Some(Modal::ConfirmRemoveWorkspace {
            id: ws.id,
            name: ws.name,
            live,
            yes_focused: true,
        });
        self.focus = Focus::Modal;
        self.status.clear();
        Ok(())
    }

    fn open_delete_session(&mut self) -> Result<()> {
        let Some(s) = self.session_list.get(self.selected_session).cloned() else {
            self.status = "No session to delete".into();
            return Ok(());
        };
        let Some(path) = s.path.clone() else {
            self.status = "Session has no disk file yet (close with x, or wait for uuid)".into();
            return Ok(());
        };
        if !path.exists() {
            self.status = format!("Session file missing: {}", path.display());
            return Ok(());
        }
        self.modal = Some(Modal::ConfirmDeleteSession {
            id: s.id,
            title: s.title,
            path,
            live: s.live,
            yes_focused: true,
        });
        self.focus = Focus::Modal;
        self.status.clear();
        Ok(())
    }

    fn confirm_delete_session(&mut self, id: &str, title: &str, path: &Path) -> Result<()> {
        if self.focused_session_id.as_deref() == Some(id) && self.focus == Focus::Agent {
            self.enter_nav()?;
        }
        let was_live = self.sessions.is_live(id);
        if was_live {
            self.sessions.close_session(id);
        }
        if self.focused_session_id.as_deref() == Some(id) {
            self.focused_session_id = None;
        }
        match delete_session_with_artifacts(path) {
            Ok(()) => {
                self.refresh_sessions();
                if self.selected_session >= self.session_list.len() {
                    self.selected_session = self.session_list.len().saturating_sub(1);
                }
                self.status = if was_live {
                    format!("Deleted session {title} (closed live first)")
                } else {
                    format!("Deleted session {title}")
                };
            }
            Err(e) => {
                self.refresh_sessions();
                self.status = format!("delete session: {e:#}");
            }
        }
        self.focus = Focus::Nav;
        Ok(())
    }

    fn confirm_remove_workspace(&mut self, id: &str, name: &str) -> Result<()> {
        if self.focus == Focus::Agent {
            self.enter_nav()?;
        }
        let closed = self.sessions.close_workspace_sessions(id);
        if self
            .focused_session_id
            .as_ref()
            .is_some_and(|fid| !self.sessions.is_live(fid))
        {
            self.focused_session_id = None;
        }
        match self.workspaces.remove(id) {
            Ok(true) => {
                if self.selected_ws >= self.workspaces.list().len() {
                    self.selected_ws = self.workspaces.list().len().saturating_sub(1);
                }
                self.selected_session = 0;
                self.refresh_sessions();
                self.status = if closed > 0 {
                    format!("Removed workspace {name} (closed {closed} live)")
                } else {
                    format!("Removed workspace {name}")
                };
            }
            Ok(false) => {
                self.status = format!("Workspace already gone: {name}");
            }
            Err(e) => {
                self.status = format!("remove workspace: {e:#}");
            }
        }
        self.focus = Focus::Nav;
        Ok(())
    }

    fn move_session(&mut self, delta: i32) {
        if self.session_list.is_empty() {
            return;
        }
        let len = self.session_list.len() as i32;
        let next = (self.selected_session as i32 + delta).rem_euclid(len) as usize;
        self.selected_session = next;
    }

    fn move_workspace(&mut self, delta: i32) {
        let list = self.workspaces.list();
        if list.is_empty() {
            return;
        }
        let len = list.len() as i32;
        self.selected_ws = (self.selected_ws as i32 + delta).rem_euclid(len) as usize;
        self.selected_session = 0;
        self.refresh_sessions();
    }

    fn refresh_sessions(&mut self) {
        let selected_id = self
            .session_list
            .get(self.selected_session)
            .map(|s| s.id.clone());
        self.session_list.clear();
        if let Some(ws) = self.workspaces.list().get(self.selected_ws) {
            match self.sessions.list_for_workspace(&ws.id, Path::new(&ws.path)) {
                Ok(list) => self.session_list = list,
                Err(e) => self.status = format!("list sessions: {e}"),
            }
        }
        // omp /fork|/branch rebinds the same PTY to a new jsonl — follow it.
        let mut select_id = selected_id;
        for (old, new) in self.sessions.drain_rebinds() {
            if self.focused_session_id.as_deref() == Some(old.as_str()) {
                self.focused_session_id = Some(new.clone());
            }
            if select_id.as_deref() == Some(old.as_str()) {
                select_id = Some(new);
            }
        }
        // Prefer focused session for selection (covers fork rebind + attach).
        let prefer = self.focused_session_id.clone().or(select_id);
        if let Some(id) = prefer {
            if let Some(i) = self.session_list.iter().position(|s| s.id == id) {
                self.selected_session = i;
            } else if self.selected_session >= self.session_list.len() {
                self.selected_session = self.session_list.len().saturating_sub(1);
            }
        } else if self.selected_session >= self.session_list.len() {
            self.selected_session = self.session_list.len().saturating_sub(1);
        }
        self.ensure_session_watch();
        self.last_title_poll = Instant::now();
    }

    fn attach_selected(&mut self) -> Result<()> {
        let Some(ws) = self.workspaces.list().get(self.selected_ws).cloned() else {
            self.status = "Add a workspace first (a)".into();
            return Ok(());
        };
        let Some(summary) = self.session_list.get(self.selected_session).cloned() else {
            self.status = "No session — press n to create".into();
            return Ok(());
        };
        // Live non-Exited + already focused → enter_agent only (B3).
        let already_live = summary.live
            && summary.status != SessionStatus::Exited
            && self.focused_session_id.as_deref() == Some(summary.id.as_str())
            && self
                .sessions
                .get(&summary.id)
                .is_some_and(|pty| !pty.is_exited());
        if already_live {
            return self.enter_agent();
        }
        let area = self.pty_area;
        let rows = area.height.max(10);
        let cols = area.width.max(40);
        // Leave previous agent hygiene if switching
        if self.focus == Focus::Agent {
            self.enter_nav()?;
        }
        let id = match self.sessions.attach_resume(
            &ws.id,
            Path::new(&ws.path),
            &summary.id,
            &summary.title,
            summary.title_kind,
            rows,
            cols,
        ) {
            Ok(id) => id,
            Err(e) => {
                // Recoverable (omp missing / session occupied / spawn fail):
                // report in-app and keep the sidebar usable instead of
                // unwinding → amux exit → killing every live child. (§8)
                self.status = format!("attach: {e:#}");
                self.refresh_sessions();
                return Ok(());
            }
        };
        self.focused_session_id = Some(id);
        self.refresh_sessions();
        self.enter_agent()?;
        Ok(())
    }

    fn new_session(&mut self) -> Result<()> {
        let Some(ws) = self.workspaces.list().get(self.selected_ws).cloned() else {
            self.status = "Add a workspace first (a)".into();
            return Ok(());
        };
        let area = self.pty_area;
        let rows = area.height.max(10);
        let cols = area.width.max(40);
        let id = match self
            .sessions
            .attach_new(&ws.id, Path::new(&ws.path), rows, cols)
        {
            Ok(id) => id,
            Err(e) => {
                self.status = format!("new session: {e:#}");
                return Ok(());
            }
        };
        self.focused_session_id = Some(id);
        self.refresh_sessions();
        // Select the new one
        if let Some(pos) = self
            .session_list
            .iter()
            .position(|s| self.focused_session_id.as_deref() == Some(&s.id))
        {
            self.selected_session = pos;
        }
        self.enter_agent()?;
        Ok(())
    }

    fn check_startup_drops(&mut self) {
        let Some(id) = self.focused_session_id.clone() else {
            return;
        };
        let Some(pty) = self.sessions.get(&id) else {
            return;
        };
        // Accumulate drops across frames; show the total once when ready.
        // (§4.2.5 — "show once N keystrokes dropped during omp startup")
        self.total_dropped_keys += pty.take_dropped_keys();
        self.total_write_drops += pty.take_write_drops();
        if let Some(at) = self.startup_hint_at {
            if pty.is_ready() {
                self.startup_hint_at = None;
                // Show cumulative drop count once at readiness transition.
                if self.total_dropped_keys > 0 {
                    self.drop_notice = Some(format!(
                        "{} keystrokes dropped during omp startup",
                        self.total_dropped_keys
                    ));
                    self.total_dropped_keys = 0;
                }
                if self.total_write_drops > 0 {
                    self.drop_notice = Some(format!(
                        "{} writes dropped (PTY backpressure)",
                        self.total_write_drops
                    ));
                    self.total_write_drops = 0;
                }
            } else if at.elapsed() > Duration::from_secs(20) {
                // Distinct 20s escalation: offer an explicit kill/retry.
                // (§4.2.5 — no hard auto-fail at 15s; 5s hint + 20s offer)
                self.status = "omp not ready after 20s — x=close · n=new retry".into();
            } else if at.elapsed() > Duration::from_secs(5) {
                self.status = "omp still starting… (20s: consider kill/retry with x)".into();
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let size = f.area();
        let collapse = size.width < 60;
        let sidebar_w = if collapse {
            0
        } else {
            (size.width * 28 / 100).clamp(24, 40)
        };
        self.sidebar_width = sidebar_w;
        self.sidebar_collapsed = collapse;

        // Footer: TOP border + hint row + status row (dux-style).
        const STATUS_H: u16 = 3;
        let chunks = if sidebar_w == 0 {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(STATUS_H)])
                .split(size)
        } else {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(sidebar_w), Constraint::Min(10)])
                .split(Rect {
                    x: size.x,
                    y: size.y,
                    width: size.width,
                    height: size.height.saturating_sub(STATUS_H),
                });
            self.draw_sidebar(f, body[0]);
            self.pty_area = body[1];
            self.draw_pty(f, body[1]);
            let status_area = Rect {
                x: size.x,
                y: size.y + size.height.saturating_sub(STATUS_H),
                width: size.width,
                height: STATUS_H,
            };
            self.draw_status(f, status_area);
            if let Some(modal) = &self.modal {
                // clone-free draw via match on type
                let _ = modal;
            }
            self.draw_modal(f, size);
            return;
        };

        // collapsed: only pty + status — no sidebar hits
        self.sidebar_rect = Rect::default();
        self.ws_list_rect = Rect::default();
        self.sess_list_rect = Rect::default();
        self.pty_area = chunks[0];
        self.draw_pty(f, chunks[0]);
        self.draw_status(f, chunks[1]);
        self.draw_modal(f, size);
    }

    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let focused = self.focus == Focus::Nav;
        // Bright cyan only while the left pane is active (Nav).
        let section_title = if focused {
            Style::default()
                .fg(t.title_focused)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.title_normal)
        };
        let block = Block::default()
            .title(Span::styled(
                match self.focus {
                    Focus::Nav => " Workspaces [Nav] ",
                    Focus::Agent | Focus::Modal => " Workspaces ",
                },
                section_title,
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused {
                t.border_focused
            } else {
                t.border_normal
            }))
            .style(Style::default().bg(t.app_bg).fg(t.text_fg));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Min(3)])
            .split(inner);

        // Nerd Font: md-folder / md-folder-open (no chevron — selection bg is enough).
        const ICON_FOLDER: &str = "󰉋 ";
        const ICON_FOLDER_OPEN: &str = "󰉖 ";

        let ws_items: Vec<ListItem> = self
            .workspaces
            .list()
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let selected = i == self.selected_ws;
                let icon = if selected {
                    ICON_FOLDER_OPEN
                } else {
                    ICON_FOLDER
                };
                let icon_style = if selected {
                    Style::default()
                        .fg(t.selection_fg)
                        .bg(t.selection_bg)
                } else {
                    Style::default().fg(t.project_icon)
                };
                let style = if selected {
                    Style::default()
                        .fg(t.selection_fg)
                        .bg(t.selection_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.text_fg)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(icon, icon_style),
                    Span::styled(w.name.as_str(), style),
                ]))
            })
            .collect();
        f.render_widget(List::new(ws_items), chunks[0]);

        let sess_block = Block::default()
            .title(Span::styled(" Sessions ", section_title))
            .borders(Borders::TOP)
            .border_style(Style::default().fg(t.border_normal));
        let sess_inner = sess_block.inner(chunks[1]);
        let row_w = sess_inner.width as usize;
        f.render_widget(sess_block, chunks[1]);

        let sess_items: Vec<ListItem> = self
            .session_list
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let selected = i == self.selected_session;
                let (dot, dot_fg) = if s.live {
                    match s.status {
                        SessionStatus::Running | SessionStatus::Starting => {
                            ("●", t.session_active)
                        }
                        SessionStatus::Exited => ("○", t.session_exited),
                        _ => ("◐", t.session_detached),
                    }
                } else {
                    ("○", t.session_exited)
                };
                // Selection chrome wins over "focused/live" tint — same cyan
                // block as the workspace list (otherwise Agent-focused row
                // only got grey text and looked unselected).
                let style = if selected {
                    Style::default()
                        .fg(t.selection_fg)
                        .bg(t.selection_bg)
                        .add_modifier(Modifier::BOLD)
                } else if Some(&s.id) == self.focused_session_id.as_ref() {
                    Style::default().fg(t.session_active)
                } else if s.title_kind == TitleKind::Provisional {
                    Style::default().fg(t.hint_desc_fg)
                } else {
                    Style::default().fg(t.text_fg)
                };
                let meta_style = if selected {
                    Style::default()
                        .fg(t.selection_fg)
                        .bg(t.selection_bg)
                } else {
                    Style::default().fg(t.hint_dim_desc_fg)
                };
                let fork_style = if selected {
                    Style::default()
                        .fg(t.selection_fg)
                        .bg(t.selection_bg)
                } else {
                    Style::default().fg(t.session_detached)
                };
                let rt = relative_time(s.mtime);
                // Right side: [fork] [time] — time furthest right (omp resume UI).
                let right = if s.is_fork {
                    format!(" fork  {rt}")
                } else {
                    format!(" {rt}")
                };
                let prefix = format!("{dot} ");
                let prefix_w = unicode_width::UnicodeWidthStr::width(prefix.as_str());
                let right_w = unicode_width::UnicodeWidthStr::width(right.as_str());
                let title_budget = row_w.saturating_sub(prefix_w).saturating_sub(right_w);
                let title = truncate_to_width(&s.title, title_budget);
                let title_w = unicode_width::UnicodeWidthStr::width(title.as_str());
                let pad = title_budget.saturating_sub(title_w);
                let mut spans = vec![
                    Span::styled(prefix, Style::default().fg(dot_fg)),
                    Span::styled(title, style),
                ];
                if pad > 0 {
                    spans.push(Span::styled(
                        " ".repeat(pad),
                        if selected {
                            Style::default().bg(t.selection_bg)
                        } else {
                            Style::default()
                        },
                    ));
                }
                if s.is_fork {
                    spans.push(Span::styled(" fork ", fork_style));
                    spans.push(Span::styled(format!(" {rt}"), meta_style));
                } else {
                    spans.push(Span::styled(format!(" {rt}"), meta_style));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        f.render_widget(List::new(sess_items), sess_inner);

        // Geometry for mouse hit-test (B4): outer sidebar, ws list, TOP inner.
        self.sidebar_rect = area;
        self.ws_list_rect = chunks[0];
        self.sess_list_rect = sess_inner;
    }

    fn draw_pty(&mut self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let focused = self.focus == Focus::Agent;
        // Browse selected session; fall back to focused (after fork rebind etc.).
        let view = self
            .session_list
            .get(self.selected_session)
            .cloned()
            .or_else(|| {
                let id = self.focused_session_id.as_ref()?;
                self.session_list.iter().find(|s| s.id == *id).cloned()
            });
        let title = match view.as_ref().map(|s| s.id.as_str()) {
            Some(id) => {
                let short = if id.len() > 36 {
                    format!("{}…", &id[..35])
                } else {
                    id.to_string()
                };
                format!(" Agent · {short} ")
            }
            None => " Agent (no session) ".into(),
        };
        let title_style = if focused {
            Style::default()
                .fg(t.title_focused)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.title_normal)
        };
        let block = Block::default()
            .title(Span::styled(title, title_style))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused {
                t.border_focused
            } else {
                t.border_normal
            }))
            .style(Style::default().bg(t.app_bg));
        let inner = block.inner(area);
        self.pty_area = inner;
        f.render_widget(block, area);

        let Some(summary) = view else {
            let mut spans = vec![self.theme.desc_span("Select a session and press ")];
            spans.extend(self.theme.key_badge("Enter"));
            spans.push(self.theme.desc_span(" to attach, or "));
            spans.extend(self.theme.key_badge("n"));
            spans.push(self.theme.desc_span(" for a new omp session."));
            f.render_widget(
                Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
                inner,
            );
            return;
        };

        let id = summary.id.clone();
        let running = self
            .sessions
            .get(&id)
            .is_some_and(|pty| !pty.is_exited());

        if running {
            self.draw_live_pty(f, inner, &id);
            return;
        }

        // Not running: JSONL transcript preview (omp-like), optional exited banner.
        let exited = self
            .sessions
            .get(&id)
            .is_some_and(|pty| pty.is_exited());
        let mut body = inner;
        if exited {
            let banner_area = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1.min(inner.height),
            };
            let mut spans = vec![Span::styled(
                "Session exited · ",
                Style::default().fg(Color::Red),
            )];
            spans.extend(self.theme.key_badge("Enter"));
            spans.push(Span::styled(
                " re-attach · ",
                Style::default().fg(Color::Red),
            ));
            spans.extend(self.theme.key_badge("x"));
            spans.push(Span::styled(" close", Style::default().fg(Color::Red)));
            f.render_widget(Paragraph::new(Line::from(spans)), banner_area);
            if inner.height > 1 {
                body = Rect {
                    x: inner.x,
                    y: inner.y + 1,
                    width: inner.width,
                    height: inner.height - 1,
                };
            } else {
                return;
            }
        }

        let Some(path) = summary.path.clone() else {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "No session file yet.",
                    Style::default().fg(t.hint_desc_fg),
                )),
                body,
            );
            return;
        };
        self.ensure_transcript_cache(&path, summary.mtime, summary.size, summary.provider);
        self.draw_transcript_preview(f, body);
    }

    fn draw_live_pty(&mut self, f: &mut Frame, inner: Rect, id: &str) {
        if let Some(pty) = self.sessions.get(id) {
            let (lr, lc) = pty.last_size();
            if lr != inner.height || lc != inner.width {
                let _ = pty.resize(inner.height, inner.width);
            }
        }
        let Some(pty) = self.sessions.get(id) else {
            return;
        };
        let snap = pty.snapshot();
        let place_cursor = self.focus == Focus::Agent
            && snap.cursor_visible
            && snap
                .cursor
                .as_ref()
                .is_some_and(|c| c.row < inner.height && c.col < inner.width);
        let cursor_pos = snap
            .cursor
            .as_ref()
            .map(|c| (inner.x + c.col, inner.y + c.row));
        {
            let buf = f.buffer_mut();
            for cell in &snap.cells {
                if cell.row >= inner.height || cell.col >= inner.width {
                    continue;
                }
                let x = inner.x + cell.col;
                let y = inner.y + cell.row;
                let ratatui_cell = &mut buf[(x, y)];
                ratatui_cell.set_symbol(&cell.symbol);
                ratatui_cell.set_style(
                    Style::default()
                        .fg(cell.fg)
                        .bg(cell.bg)
                        .add_modifier(cell.modifier),
                );
            }
            if place_cursor {
                if let Some((cx, cy)) = cursor_pos {
                    buf[(cx, cy)].set_style(
                        Style::default()
                            .fg(self.theme.input_cursor_fg)
                            .bg(self.theme.input_cursor_bg),
                    );
                }
            }
        }
        if place_cursor {
            if let Some((cx, cy)) = cursor_pos {
                f.set_cursor_position((cx, cy));
            }
        }
    }

    fn ensure_transcript_cache(
        &mut self,
        path: &Path,
        mtime: DateTime<Utc>,
        size: u64,
        provider: &str,
    ) {
        let fresh = self
            .transcript_cache
            .as_ref()
            .is_some_and(|c| c.path == path && c.mtime == mtime && c.size == size);
        if fresh {
            return;
        }
        let blocks = load(provider, path);
        self.transcript_cache = Some(TranscriptCache {
            path: path.to_path_buf(),
            mtime,
            size,
            blocks,
        });
    }

    fn draw_transcript_preview(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let Some(cache) = &self.transcript_cache else {
            return;
        };
        let h = area.height as usize;
        if h == 0 {
            return;
        }
        let rendered = render_blocks(&cache.blocks, area.width as usize, t);
        let total = rendered.len();
        let start = total.saturating_sub(h);
        let mut lines: Vec<Line> = Vec::with_capacity(h);
        for rl in rendered.iter().skip(start) {
            lines.push(rendered_line_to_ratatui(t, rl, area.width));
        }
        // Pad top if fewer lines than height (keep content at bottom like a chat).
        while lines.len() < h {
            lines.insert(0, Line::from(""));
        }
        f.render_widget(Paragraph::new(lines).style(Style::default().bg(t.app_bg)), area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(t.border_normal))
            .style(Style::default().bg(t.app_bg));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height == 0 {
            return;
        }

        let [hints_area, status_area] = if inner.height >= 2 {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .areas(inner)
        } else {
            [inner, Rect::default()]
        };

        let hints: &[(&str, &str)] = match self.focus {
            Focus::Agent => &[("Ctrl-\\", "nav")],
            Focus::Nav | Focus::Modal => &[
                ("Ctrl-\\", "agent"),
                ("a", "add workspace"),
                ("n", "new session"),
                ("Enter", "attach"),
                ("j/k", "session"),
                ("J/K", "workspace"),
                ("D", "del ws"),
                ("d", "del sess"),
                ("x", "close"),
                ("?", "help"),
                ("q", "quit"),
            ],
        };
        let hint_spans = fit_hint_spans(t, hints, hints_area.width as usize);
        f.render_widget(
            Paragraph::new(Line::from(hint_spans)).style(Style::default().bg(t.app_bg)),
            hints_area,
        );

        if status_area.height == 0 {
            return;
        }
        let line = self.powerline_status_line(status_area.width as usize);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(t.app_bg)),
            status_area,
        );
    }

    /// tmux/vim-style powerline: MODE ▸ session ▸ msg …… workspace (right)
    fn powerline_status_line(&self, max_w: usize) -> Line<'static> {
        let t = &self.theme;
        let (mode_label, mode_bg) = match self.focus {
            Focus::Agent => (" AGENT ", t.status_mode_agent_bg),
            Focus::Nav => (" NAV ", t.status_mode_shell_bg),
            Focus::Modal => (" MODAL ", t.status_mode_modal_bg),
        };

        let mut left: Vec<(String, Color, Color)> = Vec::new();
        left.push((mode_label.into(), t.status_mode_fg, mode_bg));

        if let Some(id) = &self.focused_session_id {
            let name = self
                .session_list
                .iter()
                .find(|s| s.id == *id)
                .map(|s| s.title.as_str())
                .unwrap_or(id.as_str());
            let short = if name.chars().count() > 24 {
                format!("{}…", name.chars().take(23).collect::<String>())
            } else {
                name.to_string()
            };
            left.push((format!(" {short} "), t.status_seg_fg, t.status_seg_b_bg));
        }

        let mut msg = String::new();
        if !self.status.is_empty() {
            msg.push_str(&self.status);
        }
        if let Some(ref n) = self.drop_notice {
            if !msg.is_empty() {
                msg.push_str(" · ");
            }
            msg.push_str(n);
        }
        if !self.kb.kitty && !self.kb.modify_other_keys {
            if !msg.is_empty() {
                msg.push_str(" · ");
            }
            msg.push_str("no Shift+Enter");
        }
        if self.sidebar_collapsed {
            if !msg.is_empty() {
                msg.push_str(" · ");
            }
            msg.push_str("sidebar hidden");
        }
        if !msg.is_empty() {
            left.push((format!(" {msg} "), t.status_msg_fg, t.status_msg_bg));
        }

        let mut right: Vec<(String, Color, Color)> = Vec::new();
        if let Some(ws) = self.workspaces.list().get(self.selected_ws) {
            right.push((format!(" {} ", ws.name), t.status_ws_fg, t.status_ws_bg));
        }

        render_powerline(&left, &right, max_w, t.app_bg)
    }

    fn draw_modal(&self, f: &mut Frame, area: Rect) {
        let Some(modal) = &self.modal else {
            return;
        };
        match modal {
            Modal::Help => {
                draw_help_overlay(f, area, &self.theme);
            }
            Modal::ConfirmQuit { yes_focused } => {
                draw_confirm_dialog(
                    f,
                    area,
                    &self.theme,
                    " Quit ",
                    &["Kill all live omp sessions and exit amux?"],
                    *yes_focused,
                );
            }
            Modal::ConfirmRemoveWorkspace {
                name,
                live,
                yes_focused,
                ..
            } => {
                let mut body = vec![
                    format!("Remove workspace \"{name}\" from amux?"),
                    "omp files on disk are kept.".into(),
                ];
                if *live > 0 {
                    body.push(format!("Closes {live} live session(s) first."));
                }
                let refs: Vec<&str> = body.iter().map(|s| s.as_str()).collect();
                draw_confirm_dialog(
                    f,
                    area,
                    &self.theme,
                    " Remove workspace ",
                    &refs,
                    *yes_focused,
                );
            }
            Modal::ConfirmDeleteSession {
                title,
                live,
                yes_focused,
                ..
            } => {
                let mut body = vec![
                    format!("Delete session \"{title}\"?"),
                    "Removes jsonl + artifacts on disk (omp).".into(),
                ];
                if *live {
                    body.push("Closes live PTY first.".into());
                }
                let refs: Vec<&str> = body.iter().map(|s| s.as_str()).collect();
                draw_confirm_dialog(
                    f,
                    area,
                    &self.theme,
                    " Delete session ",
                    &refs,
                    *yes_focused,
                );
            }
            Modal::DirBrowser(browser) => {
                draw_dir_browser(f, area, &self.theme, browser);
            }
        }
    }
}

/// dux-style Help: dim scrim + dark panel + cyan banners + key rows + footer.
fn draw_help_overlay(f: &mut Frame, area: Rect, theme: &Theme) {
    f.render_widget(
        Block::default().style(Style::default().bg(theme.overlay_dim_bg)),
        area,
    );

    let w = (area.width * 72 / 100).clamp(40, 88);
    let h = (area.height * 70 / 100).clamp(16, area.height.saturating_sub(2).max(16));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);

    let panel_bg = theme.help_panel_bg;
    let block = Block::default()
        .title(Span::styled(
            " Help ",
            Style::default()
                .fg(theme.text_fg)
                .bg(panel_bg)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.overlay_border).bg(panel_bg))
        .style(Style::default().bg(panel_bg).fg(theme.text_fg));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 3 || inner.width < 8 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);
    let content = chunks[0];
    let hint = chunks[1];
    let cw = content.width as usize;

    let banner_style = Style::default()
        .fg(theme.help_banner_fg)
        .bg(theme.help_banner_bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(theme.help_body_fg).bg(panel_bg);
    let section_style = Style::default()
        .fg(theme.help_section_fg)
        .bg(panel_bg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let desc_style = Style::default().fg(theme.hint_desc_fg).bg(panel_bg);

    let push_banner = |lines: &mut Vec<Line>, title: &str| {
        let pad = cw.saturating_sub(title.chars().count() + 2);
        let text = format!(" {title}{}", " ".repeat(pad));
        if !lines.is_empty() {
            lines.push(Line::from(Span::styled("", body_style)));
        }
        lines.push(Line::from(Span::styled(text, banner_style)));
        lines.push(Line::from(Span::styled("", body_style)));
    };

    let help_key_row = |key: &str, desc: &str| -> Line<'static> {
        let pad = 14usize.saturating_sub(key.len() + 2);
        let mut spans = vec![Span::styled("  ", Style::default().bg(panel_bg))];
        spans.push(Span::styled(
            "<",
            Style::default().fg(theme.hint_bracket_fg).bg(panel_bg),
        ));
        spans.push(Span::styled(
            key.to_string(),
            Style::default()
                .fg(theme.hint_key_fg)
                .bg(panel_bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            ">",
            Style::default().fg(theme.hint_bracket_fg).bg(panel_bg),
        ));
        spans.push(Span::styled(
            " ".repeat(pad),
            Style::default().bg(panel_bg),
        ));
        spans.push(Span::styled(desc.to_string(), desc_style));
        Line::from(spans)
    };

    let mut lines: Vec<Line> = Vec::new();
    push_banner(&mut lines, "About amux");
    for s in [
        "amux is a tmux-like control plane for omp coding-agent sessions.",
        "Workspaces map to project dirs; each session is a real PTY.",
        "Nav browses workspaces/sessions; Agent forwards keys to omp.",
    ] {
        lines.push(Line::from(Span::styled(s.to_string(), body_style)));
    }

    push_banner(&mut lines, "Keybindings");
    lines.push(Line::from(Span::styled("Navigation", section_style)));
    for (k, d) in [
        ("Ctrl-\\", "toggle Nav ↔ Agent (double-tap: literal to omp)"),
        ("j/k", "move session · K/J move workspace"),
        ("Enter", "attach / resume selected session"),
        ("click", "select workspace/session · Agent pane attaches"),
        ("Esc", "close this help"),
    ] {
        lines.push(help_key_row(k, d));
    }
    lines.push(Line::from(Span::styled("", body_style)));
    lines.push(Line::from(Span::styled("Workspace & session", section_style)));
    for (k, d) in [
        ("a", "add workspace (browser: o add · / search · g path)"),
        ("D", "remove workspace (confirm; keeps omp files on disk)"),
        ("n", "new omp session"),
        ("d", "delete session (confirm; jsonl + artifacts)"),
        ("x", "close live session (keep disk)"),
        ("q", "quit amux (confirm)"),
    ] {
        lines.push(help_key_row(k, d));
    }

    push_banner(&mut lines, "Notes");
    lines.push(Line::from(Span::styled(
        "AgentMode: keys go to omp (Ctrl+C, Ctrl+B, …). Nav keys never reach omp.",
        body_style,
    )));
    lines.push(Line::from(Span::styled(
        "Bracketed paste in Nav is ignored so clipboard dumps cannot fire shortcuts.",
        body_style,
    )));

    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(panel_bg))
            .wrap(Wrap { trim: false }),
        content,
    );

    // Footer hint bar (dux-style top border).
    let hint_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border_normal).bg(panel_bg))
        .style(Style::default().bg(panel_bg));
    let hint_inner = hint_block.inner(hint);
    f.render_widget(hint_block, hint);
    let mut hint_spans = vec![Span::styled(" ", Style::default().bg(panel_bg))];
    hint_spans.push(Span::styled(
        "<",
        Style::default().fg(theme.hint_bracket_fg).bg(panel_bg),
    ));
    hint_spans.push(Span::styled(
        "Esc",
        Style::default()
            .fg(theme.hint_key_fg)
            .bg(panel_bg)
            .add_modifier(Modifier::BOLD),
    ));
    hint_spans.push(Span::styled(
        ">",
        Style::default().fg(theme.hint_bracket_fg).bg(panel_bg),
    ));
    hint_spans.push(Span::styled(
        " close help",
        Style::default().fg(theme.hint_dim_desc_fg).bg(panel_bg),
    ));
    f.render_widget(
        Paragraph::new(Line::from(hint_spans)).style(Style::default().bg(panel_bg)),
        hint_inner,
    );
}

/// Powerline separators (Nerd Font / Powerline glyphs, same as tmux/vim).
const POWERLINE_RIGHT: &str = "\u{e0b0}"; // 
const POWERLINE_LEFT: &str = "\u{e0b2}"; // 

fn seg_width(segs: &[(String, Color, Color)]) -> usize {
    if segs.is_empty() {
        return 0;
    }
    segs.iter().map(|(t, _, _)| t.chars().count()).sum::<usize>() + segs.len().saturating_sub(1)
}

/// Left powerline chain + filler + right-aligned chain (workspace).
fn render_powerline(
    left: &[(String, Color, Color)],
    right: &[(String, Color, Color)],
    max_w: usize,
    fill_bg: Color,
) -> Line<'static> {
    if max_w == 0 {
        return Line::from("");
    }

    let right_w = if right.is_empty() {
        0
    } else {
        1 + seg_width(right) // leading 
    };
    let left_budget = max_w.saturating_sub(right_w);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut last_bg = fill_bg;
    let mut left_complete = true;

    for (i, (text, fg, bg)) in left.iter().enumerate() {
        let body_w = text.chars().count();
        let need_arrow = i + 1 < left.len();
        let arrow_w = usize::from(need_arrow);
        if used + body_w + arrow_w > left_budget {
            let remain = left_budget.saturating_sub(used);
            if remain > 1 {
                spans.push(Span::styled(
                    truncate_display(text, remain.saturating_sub(1)),
                    Style::default().fg(*fg).bg(*bg),
                ));
                last_bg = *bg;
                used = left_budget;
            }
            left_complete = false;
            break;
        }
        spans.push(Span::styled(
            text.clone(),
            Style::default().fg(*fg).bg(*bg),
        ));
        used += body_w;
        last_bg = *bg;
        if need_arrow {
            let next_bg = left[i + 1].2;
            spans.push(Span::styled(
                POWERLINE_RIGHT,
                Style::default().fg(*bg).bg(next_bg),
            ));
            used += 1;
        }
    }

    //  into filler, then pad
    if left_complete && !left.is_empty() && used < left_budget {
        spans.push(Span::styled(
            POWERLINE_RIGHT,
            Style::default().fg(last_bg).bg(fill_bg),
        ));
        used += 1;
    }
    let pad = max_w.saturating_sub(used + right_w);
    if pad > 0 {
        spans.push(Span::styled(
            " ".repeat(pad),
            Style::default().bg(fill_bg),
        ));
        used += pad;
    }

    if !right.is_empty() {
        let first_bg = right[0].2;
        spans.push(Span::styled(
            POWERLINE_LEFT,
            Style::default().fg(first_bg).bg(fill_bg),
        ));
        used += 1;
        for (i, (text, fg, bg)) in right.iter().enumerate() {
            spans.push(Span::styled(
                text.clone(),
                Style::default().fg(*fg).bg(*bg),
            ));
            used += text.chars().count();
            if i + 1 < right.len() {
                let next_bg = right[i + 1].2;
                spans.push(Span::styled(
                    POWERLINE_LEFT,
                    Style::default().fg(next_bg).bg(*bg),
                ));
                used += 1;
            }
        }
    }

    if used < max_w {
        spans.push(Span::styled(
            " ".repeat(max_w - used),
            Style::default().bg(fill_bg),
        ));
    }
    Line::from(spans)
}

/// Pack hint badges left-to-right; if the next pair won't fit, append `…` (dux).
fn fit_hint_spans<'a>(theme: &'a Theme, hints: &[(&'a str, &'a str)], max_w: usize) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (key, desc) in hints {
        // hint_pair: `<key>` + ` ` + desc + `  `
        let hint_w = key.len() + desc.len() + 5;
        if used + hint_w > max_w {
            if used < max_w {
                spans.push(Span::styled(
                    "…",
                    Style::default().fg(theme.hint_desc_fg).bg(theme.app_bg),
                ));
            }
            break;
        }
        spans.extend(theme.hint_pair(key, desc));
        used += hint_w;
    }
    spans
}

fn truncate_display(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let keep = max - 1;
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Compact GUI-style confirm: snug box + `[ Yes ]` / `[ No ]` buttons.
/// Keyboard only — ←/→/Tab move focus, Enter activates, y/n shortcuts.
fn confirm_key(seq: &[u8], yes_focused: &mut bool) -> ConfirmResult {
    match seq {
        b"y" | b"Y" => ConfirmResult::Yes,
        b"n" | b"N" | b"\x1b" => ConfirmResult::No,
        b"\r" | b"\n" | b" " => {
            if *yes_focused {
                ConfirmResult::Yes
            } else {
                ConfirmResult::No
            }
        }
        // Left / Shift-Tab → Yes
        b"\x1b[D" | b"\x1b[Z" | b"h" | b"H" => {
            *yes_focused = true;
            ConfirmResult::Keep
        }
        // Right / Tab → No
        b"\x1b[C" | b"\t" | b"l" | b"L" => {
            *yes_focused = false;
            ConfirmResult::Keep
        }
        _ => ConfirmResult::No,
    }
}

/// 3-row bordered button (╭─╮ / │ label │ / ╰─╯). Focused = filled cyan.
fn draw_confirm_button(f: &mut Frame, area: Rect, theme: &Theme, label: &str, focused: bool) {
    if area.width < 5 || area.height < 3 {
        return;
    }
    let (border_fg, fill_bg, label_fg, border_type) = if focused {
        (
            theme.selection_bg,
            theme.selection_bg,
            theme.selection_fg,
            BorderType::Double,
        )
    } else {
        (
            Color::Indexed(245), // gray outline
            theme.overlay_bg,
            theme.hint_desc_fg,
            BorderType::Rounded,
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(Style::default().fg(border_fg).bg(theme.overlay_bg))
        .style(Style::default().bg(fill_bg));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Center label in the single content row.
    let label_w = unicode_width::UnicodeWidthStr::width(label) as u16;
    let pad = inner.width.saturating_sub(label_w) / 2;
    let line = Line::from(vec![
        Span::styled(
            " ".repeat(pad as usize),
            Style::default().bg(fill_bg),
        ),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(label_fg)
                .bg(fill_bg)
                .add_modifier(if focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(fill_bg)),
        inner,
    );
}

fn draw_confirm_dialog(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    body: &[&str],
    yes_focused: bool,
) {
    // Full-screen scrim so the dialog reads as a layer (RGB truecolor is
    // often ignored under TERM=xterm / WebSSH — panel uses Indexed colors).
    f.render_widget(
        Block::default().style(Style::default().bg(theme.overlay_dim_bg)),
        area,
    );

    let pad = 4u16;
    let mut content_w = unicode_width::UnicodeWidthStr::width(title.trim()) as u16;
    for line in body {
        content_w = content_w.max(unicode_width::UnicodeWidthStr::width(*line) as u16);
    }
    // Two 12-wide buttons + gap
    content_w = content_w.max(28);
    let w = (content_w + pad + 2).clamp(34, area.width.saturating_sub(4).max(34).min(58));
    // outer borders(2) + body + gap + button(3) + gap + hint(1) + pad
    let h = (body.len() as u16)
        .saturating_add(9)
        .min(area.height.saturating_sub(2).max(11));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.title_focused)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.overlay_border))
        .style(Style::default().bg(theme.overlay_bg).fg(theme.text_fg));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Block::default().style(Style::default().bg(theme.overlay_bg)),
        inner,
    );

    let body_h = (body.len() as u16).saturating_add(1).min(inner.height.saturating_sub(5));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(body_h),
            Constraint::Length(3), // bordered buttons
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // hint
        ])
        .split(inner);

    let body_lines: Vec<Line> = body
        .iter()
        .map(|s| {
            Line::from(Span::styled(
                (*s).to_string(),
                Style::default()
                    .fg(theme.text_fg)
                    .bg(theme.overlay_bg),
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(body_lines)
            .style(Style::default().bg(theme.overlay_bg))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    // Center a pair of equal buttons in the button row.
    const BTN_W: u16 = 12;
    const BTN_GAP: u16 = 3;
    let pair_w = BTN_W * 2 + BTN_GAP;
    let btn_row = chunks[1];
    let start_x = btn_row.x + btn_row.width.saturating_sub(pair_w) / 2;
    let yes_rect = Rect {
        x: start_x,
        y: btn_row.y,
        width: BTN_W.min(btn_row.width),
        height: btn_row.height.min(3),
    };
    let no_rect = Rect {
        x: start_x.saturating_add(BTN_W + BTN_GAP),
        y: btn_row.y,
        width: BTN_W.min(btn_row.width.saturating_sub(BTN_W + BTN_GAP)),
        height: btn_row.height.min(3),
    };
    draw_confirm_button(f, yes_rect, theme, "Yes", yes_focused);
    draw_confirm_button(f, no_rect, theme, "No", !yes_focused);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ←/→ or Tab · Enter confirm · y / n / Esc".to_string(),
            Style::default()
                .fg(theme.hint_dim_desc_fg)
                .bg(theme.overlay_bg),
        )))
        .style(Style::default().bg(theme.overlay_bg))
        .alignment(ratatui::layout::Alignment::Center),
        chunks[3],
    );
}

/// Compute the PTY surface *content* rect from terminal size, mirroring
/// draw_pty's Block::borders(ALL) inner area so winch can resize all live
/// PTYs — including background sessions — to the correct cell dimensions.
/// (§4.2.7.1/§4.2.7.2)
fn compute_pty_area(size: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let sidebar_w = if size.width < 60 {
        0
    } else {
        (size.width * 28 / 100).clamp(24, 40)
    };
    // Borders::ALL subtracts 1 cell per side; footer is border+hints+status.
    let border_w = 2u16;
    let border_h = 2u16;
    let status_h = 3u16;
    if sidebar_w == 0 {
        ratatui::layout::Rect {
            x: size.x,
            y: size.y,
            width: size.width.saturating_sub(border_w),
            height: size.height.saturating_sub(status_h + border_h),
        }
    } else {
        ratatui::layout::Rect {
            x: size.x + sidebar_w,
            y: size.y,
            width: size.width.saturating_sub(sidebar_w + border_w),
            height: size.height.saturating_sub(status_h + border_h),
        }
    }
}


/// Parse (cb, col, row) from an SGR mouse sequence — all 1-based.
fn parse_sgr_cxy(seq: &[u8]) -> Option<(u8, u16, u16)> {
    if !is_sgr_mouse(seq) {
        return None;
    }
    let params = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    let mut parts = params.split(';');
    let cb: u8 = parts.next()?.parse().ok()?;
    let x: u16 = parts.next()?.parse().ok()?;
    let y: u16 = parts.next()?.parse().ok()?;
    Some((cb, x, y))
}
fn parse_sgr_xy(seq: &[u8]) -> Option<(u16, u16)> {
    if !is_sgr_mouse(seq) {
        return None;
    }
    let params = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    let mut parts = params.split(';');
    let _cb = parts.next()?;
    let cx: u16 = parts.next()?.parse().ok()?;
    let cy: u16 = parts.next()?.parse().ok()?;
    Some((cx.saturating_sub(1), cy.saturating_sub(1)))
}

/// Map a rendered transcript line to ratatui, honoring inline span styles.
/// Spans carry per-fragment styling (bold/italic/code/heading/link/…); the
/// role supplies the base color and, for User, the bubble background padded
/// to the full width.
fn rendered_line_to_ratatui(t: &Theme, rl: &RenderedLine, width: u16) -> Line<'static> {
    let base = match rl.role {
        TranscriptRole::User => Style::default().fg(t.transcript_user_fg),
        TranscriptRole::Assistant => Style::default().fg(t.transcript_assistant_fg),
        TranscriptRole::Tool => Style::default().fg(t.transcript_tool_fg),
        TranscriptRole::Thinking => {
            Style::default().fg(t.transcript_thinking_fg).add_modifier(Modifier::ITALIC)
        }
        TranscriptRole::Meta => Style::default().fg(t.transcript_meta_fg),
        TranscriptRole::Custom => Style::default().fg(t.transcript_assistant_fg),
    };
    let bg = if rl.role == TranscriptRole::User {
        Some(t.transcript_user_bg)
    } else {
        None
    };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(rl.spans.len() + 1);
    let mut w = 0usize;
    for span in &rl.spans {
        let mut style = base;
        if let Some(bgc) = bg {
            style = style.bg(bgc);
        }
        match span.style {
            SpanStyle::Normal => {}
            SpanStyle::Bold => style = style.add_modifier(Modifier::BOLD),
            SpanStyle::Italic => style = style.add_modifier(Modifier::ITALIC),
            SpanStyle::Code => style = style.fg(t.md_code),
            SpanStyle::Heading => style = style.fg(t.md_heading).add_modifier(Modifier::BOLD),
            SpanStyle::Link => style = style.fg(t.md_link).add_modifier(Modifier::UNDERLINED),
            SpanStyle::Dim => style = style.fg(t.dim),
            SpanStyle::ListBullet => style = style.fg(t.md_list_bullet),
            SpanStyle::CodeBlock => style = style.fg(t.md_code_block),
            SpanStyle::StatusOk => style = style.fg(t.success),
            SpanStyle::StatusErr => style = style.fg(t.error),
            SpanStyle::StatusPending => style = style.fg(t.dim),
            SpanStyle::BashBorder => style = style.fg(t.bash_mode),
            SpanStyle::EvalBorder => style = style.fg(t.python_mode),
            SpanStyle::Accent => style = style.fg(t.accent),
            SpanStyle::CustomLabel => style = style.fg(t.custom_message_label).add_modifier(Modifier::BOLD),
        }
        w += unicode_width::UnicodeWidthStr::width(span.text.as_str());
        spans.push(Span::styled(span.text.clone(), style));
    }
    // User bubble: pad to width so the background fills the line.
    if rl.role == TranscriptRole::User {
        let pad = (width as usize).saturating_sub(w);
        if pad > 0 {
            spans.push(Span::styled(
                " ".repeat(pad),
                Style::default().fg(t.transcript_user_fg).bg(t.transcript_user_bg),
            ));
        }
    }
    Line::from(spans)
}

/// Compact relative time for session rows (e.g. "5m", "3h", "2d").
/// Avoids pulling in a humanize crate. (§6.1 sessions show relative time)

fn relative_time(t: DateTime<Utc>) -> String {
    let s = Utc::now().signed_duration_since(t).num_seconds().max(0);
    if s < 60 {
        return "now".into();
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 30 {
        return format!("{d}d");
    }
    let mo = d / 30;
    if mo < 12 {
        return format!("{mo}mo");
    }
    format!("{}y", mo / 12)
}

/// Truncate `s` to at most `max_w` terminal columns, appending `…` when cut.
fn truncate_to_width(s: &str, max_w: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if max_w == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max_w {
        return s.to_string();
    }
    if max_w == 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
        if w + cw > max_w.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn set_nonblocking(fd: i32, on: bool) -> Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    let flags = fcntl(fd, FcntlArg::F_GETFL).context("F_GETFL")?;
    let mut oflags = OFlag::from_bits_truncate(flags);
    if on {
        oflags.insert(OFlag::O_NONBLOCK);
    } else {
        oflags.remove(OFlag::O_NONBLOCK);
    }
    fcntl(fd, FcntlArg::F_SETFL(oflags)).context("F_SETFL")?;
    Ok(())
}

fn poll_fd(fd: i32, timeout: Duration) -> Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut pfd = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let to = PollTimeout::try_from(ms).unwrap_or(PollTimeout::ZERO);
    match poll(&mut pfd, to) {
        Ok(n) => Ok(n > 0),
        Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => Ok(false),
        Err(e) => Err(anyhow::Error::new(e).context("poll stdin")),
    }
}
