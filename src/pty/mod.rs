//! PtySession: portable-pty + alacritty_terminal VT + writer queue + teardown.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as TermColor, CursorShape, NamedColor, Processor, Rgb, StdSyncHandler,
};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use ratatui::style::{Color, Modifier};

use crate::appearance::{
    colorfgbg_env, mode2031_notify_bytes, palette_set_osc, Appearance, HostSurface,
};

const WRITE_QUEUE_CAP: usize = 256 * 1024;
const SYNC_POLL_MS: u64 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCursor {
    pub row: u16,
    pub col: u16,
}

#[derive(Clone, Debug)]
pub struct SnapshotCell {
    pub row: u16,
    pub col: u16,
    pub symbol: String,
    pub fg: Color,
    pub bg: Color,
    pub modifier: Modifier,
}

#[derive(Clone, Debug, Default)]
pub struct TerminalSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor: Option<SnapshotCursor>,
    pub cells: Vec<SnapshotCell>,
    pub cursor_visible: bool,
}

/// Mirrored child modes for host application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MirroredModes {
    pub bracketed_paste: bool,
    pub mouse_1000: bool,
    pub mouse_1002: bool,
    pub mouse_1003: bool,
    pub mouse_sgr: bool,
    pub focus: bool,
    pub alt_screen: bool,
    pub alt_scroll: bool,
    /// modifyOtherKeys level (0=off, 1/2=on). alacritty_terminal 0.26 silently
    /// ignores CSI > 4 sequences; we track and mirror them to the host. (§4.2.3a.4)
    pub modify_other_keys: u8,
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    write_tx: Arc<Mutex<WriteQueue>>,
    write_notify: Arc<std::sync::Condvar>,
    write_mutex: Arc<std::sync::Mutex<()>>,
    terminal: Arc<Mutex<TerminalState>>,
    child: Box<dyn Child + Send + Sync>,
    child_pid: Option<i32>,
    exited: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    dropped_keys: Arc<AtomicU64>,
    write_drops: Arc<AtomicU64>,
    last_size: Mutex<(u16, u16)>,
    writer_thread: Option<JoinHandle<()>>,
    reader_thread: Option<JoinHandle<()>>,
    sync_thread: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    host_outbound: Arc<Mutex<Vec<u8>>>,
}

struct WriteQueue {
    buf: VecDeque<u8>,
    closed: bool,
    full_since: Option<Instant>,
}

impl WriteQueue {
    fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            closed: false,
            full_since: None,
        }
    }

    fn enqueue(&mut self, bytes: &[u8]) -> Result<(), EnqueueErr> {
        if self.closed {
            return Err(EnqueueErr::Closed);
        }
        if self.buf.len() + bytes.len() > WRITE_QUEUE_CAP {
            if self.full_since.is_none() {
                self.full_since = Some(Instant::now());
            }
            return Err(EnqueueErr::Full);
        }
        self.full_since = None;
        self.buf.extend(bytes);
        Ok(())
    }

    fn drain_chunk(&mut self, max: usize) -> Vec<u8> {
        let n = max.min(self.buf.len());
        self.buf.drain(..n).collect()
    }
}

#[derive(Debug)]
enum EnqueueErr {
    Closed,
    Full,
}

impl PtySession {
    pub fn spawn(
        command: &str,
        args: &[String],
        cwd: &Path,
        rows: u16,
        cols: u16,
        env_extra: &[(String, String)],
        kitty_keyboard: bool,
        surface: HostSurface,
    ) -> Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(command);
        for a in args {
            cmd.arg(a);
        }
        cmd.cwd(cwd);
        apply_child_env(&mut cmd, env_extra, surface.appearance);

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {command}"))?;
        drop(pair.slave);

        let child_pid = child.process_id().map(|p| p as i32);

        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        let terminal = Arc::new(Mutex::new(TerminalState::new(
            rows,
            cols,
            kitty_keyboard,
            surface,
        )));
        let exited = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(true));
        let ready = Arc::new(AtomicBool::new(false));
        let dropped_keys = Arc::new(AtomicU64::new(0));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let write_tx = Arc::new(Mutex::new(WriteQueue::new()));
        let write_notify = Arc::new(std::sync::Condvar::new());
        let write_mutex = Arc::new(std::sync::Mutex::new(()));
        let host_outbound = Arc::new(Mutex::new(Vec::new()));

        // Writer thread — never block UI on PTY write.
        let wq = Arc::clone(&write_tx);
        let wn = Arc::clone(&write_notify);
        let wm = Arc::clone(&write_mutex);
        let stop_w = Arc::clone(&stop_flag);
        let writer_thread = thread::spawn(move || {
            let mut writer = writer;
            loop {
                let chunk = {
                    let mut guard = wm.lock().unwrap();
                    loop {
                        {
                            let q = wq.lock();
                            if !q.buf.is_empty() {
                                break;
                            }
                            if q.closed || stop_w.load(Ordering::Acquire) {
                                return;
                            }
                        }
                        let (g, _) = wn
                            .wait_timeout(guard, Duration::from_millis(200))
                            .unwrap();
                        guard = g;
                    }
                    wq.lock().drain_chunk(8192)
                };
                if chunk.is_empty() {
                    continue;
                }
                if writer.write_all(&chunk).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // Reader thread
        let term_r = Arc::clone(&terminal);
        let wq_r = Arc::clone(&write_tx);
        let wn_r = Arc::clone(&write_notify);
        let wm_r = Arc::clone(&write_mutex);
        let host_r = Arc::clone(&host_outbound);
        let exited_r = Arc::clone(&exited);
        let dirty_r = Arc::clone(&dirty);
        let ready_r = Arc::clone(&ready);
        let stop_r = Arc::clone(&stop_flag);
        let reader_thread = thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                if stop_r.load(Ordering::Acquire) {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => {
                        {
                            let mut t = term_r.lock();
                            t.force_stop_sync();
                        }
                        exited_r.store(true, Ordering::Release);
                        dirty_r.store(true, Ordering::Release);
                        break;
                    }
                    Ok(n) => {
                        let mut t = term_r.lock();
                        let (replies, host) = t.process(&buf[..n]);
                        if !ready_r.load(Ordering::Acquire) && t.is_ready() {
                            ready_r.store(true, Ordering::Release);
                        }
                        dirty_r.store(true, Ordering::Release);
                        drop(t);
                        if !host.is_empty() {
                            host_r.lock().extend_from_slice(&host);
                        }
                        if !replies.is_empty() {
                            let mut q = wq_r.lock();
                            let _ = q.enqueue(&replies);
                            drop(q);
                            let _g = wm_r.lock().unwrap();
                            wn_r.notify_one();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        let mut t = term_r.lock();
                        t.force_stop_sync();
                        exited_r.store(true, Ordering::Release);
                        dirty_r.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        });

        // Sync timeout thread — call stop_sync if ESU never arrives.
        let term_s = Arc::clone(&terminal);
        let dirty_s = Arc::clone(&dirty);
        let stop_s = Arc::clone(&stop_flag);
        let exited_s = Arc::clone(&exited);
        let sync_thread = thread::spawn(move || {
            while !stop_s.load(Ordering::Acquire) && !exited_s.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(SYNC_POLL_MS));
                let mut t = term_s.lock();
                if t.maybe_stop_sync_timeout() {
                    dirty_s.store(true, Ordering::Release);
                }
            }
            let mut t = term_s.lock();
            t.force_stop_sync();
        });

        Ok(Self {
            master: pair.master,
            write_tx,
            write_notify,
            write_mutex,
            terminal,
            child,
            child_pid,
            exited,
            dirty,
            ready,
            dropped_keys,
            write_drops: Arc::new(AtomicU64::new(0)),
            last_size: Mutex::new((rows, cols)),
            writer_thread: Some(writer_thread),
            reader_thread: Some(reader_thread),
            sync_thread: Some(sync_thread),
            stop_flag,
            host_outbound,
        })
    }

    /// Drain OSC52 / other host-bound escapes (write to outer stdout).
    pub fn take_host_outbound(&self) -> Vec<u8> {
        std::mem::take(&mut *self.host_outbound.lock())
    }

    pub fn enqueue_write(&self, bytes: &[u8]) -> Result<()> {
        if self.exited.load(Ordering::Acquire) {
            bail!("pty exited; refuse write");
        }
        if !self.ready.load(Ordering::Acquire) {
            // Startup gate: drop (do not flush)
            self.dropped_keys.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let mut q = self.write_tx.lock();
        match q.enqueue(bytes) {
            Ok(()) => {
                drop(q);
                let _g = self.write_mutex.lock().unwrap();
                self.write_notify.notify_one();
                self.dirty.store(true, Ordering::Release);
                Ok(())
            }
            Err(EnqueueErr::Closed) => bail!("write queue closed"),
            Err(EnqueueErr::Full) => {
                if q.full_since
                    .map(|t| t.elapsed() > Duration::from_secs(2))
                    .unwrap_or(false)
                {
                    bail!("PTY write queue full >2s");
                }
                self.write_drops.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// Bypass readiness gate (for focus CSI I/O, mouse release, etc. after ready
    /// or for intentional control). Still refuses after exit.
    pub fn enqueue_write_forced(&self, bytes: &[u8]) -> Result<()> {
        if self.exited.load(Ordering::Acquire) {
            bail!("pty exited; refuse write");
        }
        let mut q = self.write_tx.lock();
        q.enqueue(bytes).map_err(|_| anyhow::anyhow!("enqueue failed"))?;
        drop(q);
        let _g = self.write_mutex.lock().unwrap();
        self.write_notify.notify_one();
        Ok(())
    }

    pub fn take_dropped_keys(&self) -> u64 {
        self.dropped_keys.swap(0, Ordering::Relaxed)
    }

    pub fn take_write_drops(&self) -> u64 {
        self.write_drops.swap(0, Ordering::Relaxed)
    }

    /// True when the write queue is near its cap — callers should stop
    /// ingesting stdin so backpressure surfaces instead of silent drops.
    /// (§4.2.8)
    pub fn is_write_backpressured(&self) -> bool {
        let q = self.write_tx.lock();
        q.buf.len() + 32 > WRITE_QUEUE_CAP
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Best-effort child exit success — `None` if still running.
    pub fn exit_success(&mut self) -> Option<bool> {
        self.child.try_wait().ok().flatten().map(|s| s.success())
    }

    pub fn mirrored_modes(&self) -> MirroredModes {
        self.terminal.lock().mirrored_modes()
    }

    /// Scroll the emulator viewport into history (`lines > 0` = older /
    /// up). Returns `true` if `display_offset` changed (primary buffer with
    /// scrollback). Alt-screen grids have no history — returns `false`.
    pub fn scroll_display_lines(&self, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        let mut term = self.terminal.lock();
        let changed = term.scroll_display_lines(lines);
        if changed {
            self.dirty.store(true, Ordering::Release);
        }
        changed
    }

    /// Sync PTY dynamic colors with host surface and notify the child (Mode 2031 DSR)
    /// so agents like omp re-query OSC 11 and switch their own theme.
    pub fn set_host_surface(&self, surface: HostSurface) {
        let appearance = surface.appearance;
        {
            let mut term = self.terminal.lock();
            if term.surface == surface {
                return;
            }
            term.apply_host_surface(surface);
        }
        self.dirty.store(true, Ordering::Release);
        let notify = mode2031_notify_bytes(appearance);
        let _ = self.enqueue_write_force(&notify);
    }

    /// Like [`enqueue_write`] but skips the startup ready-gate (host→child theme notify).
    fn enqueue_write_force(&self, bytes: &[u8]) -> Result<()> {
        if self.exited.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut q = self.write_tx.lock();
        match q.enqueue(bytes) {
            Ok(()) => {
                drop(q);
                let _g = self.write_mutex.lock().unwrap();
                self.write_notify.notify_one();
                Ok(())
            }
            Err(EnqueueErr::Closed) => Ok(()),
            Err(EnqueueErr::Full) => {
                self.write_drops.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        // Hold the terminal lock across both resizes so the reader thread
        // cannot process data with mismatched PTY/VT dimensions (§4.2.7.3).
        let mut term = self.terminal.lock();
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        term.resize(rows, cols);
        drop(term);
        *self.last_size.lock() = (rows, cols);
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn last_size(&self) -> (u16, u16) {
        *self.last_size.lock()
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        self.terminal.lock().snapshot()
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    /// Graceful process-group teardown: SIGHUP → 300ms → SIGTERM → 500ms → SIGKILL.
    pub fn kill_process_group(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        {
            let mut q = self.write_tx.lock();
            q.closed = true;
        }
        {
            let _g = self.write_mutex.lock().unwrap();
            self.write_notify.notify_all();
        }

        if let Some(pid) = self.child_pid {
            use nix::sys::signal::{kill, killpg, Signal};
            use nix::unistd::Pid;
            let pgid = Pid::from_raw(pid);
            // Prefer process-group; fall back to direct pid if not a leader.
            if killpg(pgid, Signal::SIGHUP).is_err() {
                let _ = kill(pgid, Signal::SIGHUP);
            }
            thread::sleep(Duration::from_millis(300));
            if self.child.try_wait().ok().flatten().is_none() {
                if killpg(pgid, Signal::SIGTERM).is_err() {
                    let _ = kill(pgid, Signal::SIGTERM);
                }
                thread::sleep(Duration::from_millis(500));
            }
            if self.child.try_wait().ok().flatten().is_none() {
                if killpg(pgid, Signal::SIGKILL).is_err() {
                    let _ = kill(pgid, Signal::SIGKILL);
                }
            }
        } else {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();

        {
            let mut t = self.terminal.lock();
            t.force_stop_sync();
        }
        self.exited.store(true, Ordering::Release);

        if let Some(h) = self.sync_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.reader_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.writer_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if !self.exited.load(Ordering::Acquire) {
            self.kill_process_group();
        }
    }
}
struct TerminalState {
    term: Term<EventProxy>,
    parser: Processor<StdSyncHandler>,
    event_proxy: EventProxy,
    rows: u16,
    cols: u16,
    /// Tracked from raw child output — alacritty_terminal 0.26 ignores CSI > 4.
    modify_other_keys: u8,
    surface: HostSurface,
}

impl TerminalState {
    fn new(rows: u16, cols: u16, kitty_keyboard: bool, surface: HostSurface) -> Self {
        let event_proxy = EventProxy::new(rows, cols);
        let dimensions = TermDimensions::new(rows, cols);
        let config = Config {
            scrolling_history: 10_000,
            kitty_keyboard,
            ..Config::default()
        };
        let mut state = Self {
            term: Term::new(config, &dimensions, event_proxy.clone()),
            parser: Processor::new(),
            event_proxy,
            rows,
            cols,
            modify_other_keys: 0,
            surface,
        };
        // Seed dynamic FG/BG so child OSC 10/11 queries match host surface.
        state.apply_host_surface(surface);
        state
    }

    fn apply_host_surface(&mut self, surface: HostSurface) {
        self.surface = surface;
        let osc = palette_set_osc(surface);
        // Drive alacritty Handler::set_color (no public colors_mut API).
        let _ = self.process(&osc);
    }

    /// Returns (child_replies, host_outbound).
    fn process(&mut self, data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        self.parser.advance(&mut self.term, data);
        let pending = self.event_proxy.take_pending();
        let mut replies = pending.bytes;
        for req in pending.color_requests {
            let rgb = resolve_color_request_rgb(req.index, self.term.colors(), self.surface);
            replies.extend_from_slice((req.formatter)(rgb).as_bytes());
        }
        // ClipboardLoad MUST reply to child (do not swallow)
        for req in pending.clipboard_loads {
            replies.extend_from_slice(req.as_bytes());
        }
        // alacritty_terminal 0.26 silently ignores CSI > 4 (modifyOtherKeys).
        // Track the level from raw child output and mirror to host. (§4.2.3a.4)
        scan_modify_other_keys(data, &mut self.modify_other_keys);
        (replies, pending.host_bytes)
    }

    fn maybe_stop_sync_timeout(&mut self) -> bool {
        if let Some(deadline) = self.parser.sync_timeout().sync_timeout() {
            if Instant::now() >= deadline {
                self.parser.stop_sync(&mut self.term);
                return true;
            }
        }
        false
    }

    fn force_stop_sync(&mut self) {
        if self.parser.sync_timeout().sync_timeout().is_some() {
            self.parser.stop_sync(&mut self.term);
        }
    }

    fn is_ready(&self) -> bool {
        let mode = self.term.mode();
        if mode.contains(TermMode::ALT_SCREEN)
            || mode.contains(TermMode::BRACKETED_PASTE)
            || mode.intersects(TermMode::MOUSE_MODE)
        {
            return true;
        }
        self.term
            .renderable_content()
            .display_iter
            .any(|indexed| !indexed.cell.c.is_whitespace())
    }

    fn mirrored_modes(&self) -> MirroredModes {
        let m = self.term.mode();
        MirroredModes {
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            mouse_1000: m.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_1002: m.contains(TermMode::MOUSE_DRAG),
            mouse_1003: m.contains(TermMode::MOUSE_MOTION),
            mouse_sgr: m.contains(TermMode::SGR_MOUSE),
            focus: m.contains(TermMode::FOCUS_IN_OUT),
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            alt_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            modify_other_keys: self.modify_other_keys,
        }
    }

    /// `lines > 0` scrolls toward older history (wheel up).
    fn scroll_display_lines(&mut self, lines: i32) -> bool {
        let before = self.term.grid().display_offset();
        // alacritty: positive Delta increases display_offset (older).
        self.term.scroll_display(Scroll::Delta(lines));
        self.term.grid().display_offset() != before
    }

    fn snapshot(&self) -> TerminalSnapshot {
        let renderable = self.term.renderable_content();
        let display_offset = renderable.display_offset;
        let colors = renderable.colors;
        let cursor_visible = renderable.cursor.shape != CursorShape::Hidden;
        let cursor = if cursor_visible {
            term::point_to_viewport(display_offset, renderable.cursor.point).map(|point| {
                SnapshotCursor {
                    row: point.line as u16,
                    col: point.column.0 as u16,
                }
            })
        } else {
            None
        };

        let mut cells = Vec::new();
        for indexed in renderable.display_iter {
            let cell = indexed.cell;
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let Some(point) = term::point_to_viewport(display_offset, indexed.point) else {
                continue;
            };
            let mut symbol = String::new();
            symbol.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                for ch in zw {
                    symbol.push(*ch);
                }
            }
            // Do not forward BOLD / ITALIC / REVERSED to the outer terminal.
            // amux re-paints nested PTY cells through ratatui; under tmux those
            // SGR attrs (esp. italic on omp blockquotes) show up as solid gray
            // bars (fg/bg swap look), while direct tmux+omp does not. Emphasis
            // already lives in the cell's RGB/indexed colors — keep those.
            let mut modifier = Modifier::empty();
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                modifier |= Modifier::UNDERLINED;
            }
            if cell.flags.contains(Flags::DIM) {
                modifier |= Modifier::DIM;
            }
            cells.push(SnapshotCell {
                row: point.line as u16,
                col: point.column.0 as u16,
                symbol,
                fg: convert_color(cell.fg, colors, self.surface),
                bg: convert_color(cell.bg, colors, self.surface),
                modifier,
            });
        }

        TerminalSnapshot {
            rows: self.rows,
            cols: self.cols,
            cursor,
            cells,
            cursor_visible,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.event_proxy.set_size(rows, cols);
        self.term.resize(TermDimensions::new(rows, cols));
    }
}

struct TermDimensions {
    cols: u16,
    rows: u16,
}

impl TermDimensions {
    fn new(rows: u16, cols: u16) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

type ColorFmt = Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;

#[derive(Default)]
struct PendingEvents {
    bytes: Vec<u8>,
    host_bytes: Vec<u8>,
    color_requests: Vec<PendingColor>,
    clipboard_loads: Vec<String>,
}

struct PendingColor {
    index: usize,
    formatter: ColorFmt,
}

#[derive(Clone)]
struct EventProxy {
    pending: Arc<Mutex<PendingEvents>>,
    size: Arc<Mutex<(u16, u16)>>,
}

impl EventProxy {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            pending: Arc::new(Mutex::new(PendingEvents::default())),
            size: Arc::new(Mutex::new((rows, cols))),
        }
    }

    fn take_pending(&self) -> PendingEvents {
        std::mem::take(&mut *self.pending.lock())
    }

    fn set_size(&self, rows: u16, cols: u16) {
        *self.size.lock() = (rows, cols);
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                self.pending.lock().bytes.extend_from_slice(text.as_bytes());
            }
            Event::ColorRequest(index, formatter) => {
                self.pending.lock().color_requests.push(PendingColor {
                    index,
                    formatter,
                });
            }
            Event::TextAreaSizeRequest(formatter) => {
                let (rows, cols) = *self.size.lock();
                let response = formatter(WindowSize {
                    num_lines: rows,
                    num_cols: cols,
                    cell_width: 8,
                    cell_height: 16,
                });
                self.pending.lock().bytes.extend_from_slice(response.as_bytes());
            }
            Event::ClipboardStore(_clipboard, data) => {
                // Pass OSC 52 to outer terminal — never to child PTY.
                // alacritty decodes the child's base64 before firing this
                // event, so we must re-encode to base64 for the wire format.
                // (§4.2.6b)
                let encoded = base64_encode(data.as_bytes());
                let osc = format!("\x1b]52;c;{encoded}\x07");
                self.pending
                    .lock()
                    .host_bytes
                    .extend_from_slice(osc.as_bytes());
            }
            Event::ClipboardLoad(_clipboard, formatter) => {
                // MUST reply — empty clipboard is fine for v1.
                let reply = formatter("");
                self.pending.lock().clipboard_loads.push(reply);
            }
            _ => {}
        }
    }
}

fn apply_child_env(
    cmd: &mut CommandBuilder,
    extra: &[(String, String)],
    appearance: Appearance,
) {
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Light only: hint a light host. Dark leaves COLORFGBG alone (pre-sync behavior).
    if let Some(cfg) = colorfgbg_env(appearance) {
        cmd.env("COLORFGBG", cfg);
    }

    // Scrub identity / multiplexer vars so child does not assume host capabilities.
    const SCRUB: &[&str] = &[
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERM_FEATURES",
        "KITTY_WINDOW_ID",
        "GHOSTTY_RESOURCES_DIR",
        "ITERM_SESSION_ID",
        "VSCODE_PID",
        "ALACRITTY_WINDOW_ID",
        "WT_SESSION",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "LINES",
        "COLUMNS",
    ];
    for key in SCRUB {
        cmd.env_remove(key);
    }
    // WEZTERM_*
    for (k, _) in std::env::vars() {
        if k.starts_with("WEZTERM_") {
            cmd.env_remove(&k);
        }
    }

    for (k, v) in extra {
        // Don't let PI pins clobber the light-theme COLORFGBG we set above.
        if k == "COLORFGBG" && colorfgbg_env(appearance).is_some() {
            continue;
        }
        cmd.env(k, v);
    }
}

/// Answer OSC color queries with the live palette, else appearance-aware defaults.
/// A gray `0xc0c0c0` stub here makes agents (omp) invent a broken theme.
fn resolve_color_request_rgb(
    index: usize,
    palette: &alacritty_terminal::term::color::Colors,
    surface: HostSurface,
) -> Rgb {
    (index < alacritty_terminal::term::color::COUNT)
        .then(|| palette[index])
        .flatten()
        .or_else(|| default_palette_rgb(index, surface))
        .unwrap_or_else(|| {
            if index == NamedColor::Background as usize {
                rgb(surface.bg.0, surface.bg.1, surface.bg.2)
            } else {
                rgb(surface.fg.0, surface.fg.1, surface.fg.2)
            }
        })
}

fn default_palette_rgb(index: usize, surface: HostSurface) -> Option<Rgb> {
    let (fr, fg, fb) = surface.fg;
    let (br, bg, bb) = surface.bg;
    match index {
        0 => Some(rgb(0x00, 0x00, 0x00)),
        1 => Some(rgb(0xcd, 0x00, 0x00)),
        2 => Some(rgb(0x00, 0xcd, 0x00)),
        3 => Some(rgb(0xcd, 0xcd, 0x00)),
        4 => Some(rgb(0x00, 0x00, 0xee)),
        5 => Some(rgb(0xcd, 0x00, 0xcd)),
        6 => Some(rgb(0x00, 0xcd, 0xcd)),
        7 => Some(rgb(0xe5, 0xe5, 0xe5)),
        8 => Some(rgb(0x7f, 0x7f, 0x7f)),
        9 => Some(rgb(0xff, 0x00, 0x00)),
        10 => Some(rgb(0x00, 0xff, 0x00)),
        11 => Some(rgb(0xff, 0xff, 0x00)),
        12 => Some(rgb(0x5c, 0x5c, 0xff)),
        13 => Some(rgb(0xff, 0x00, 0xff)),
        14 => Some(rgb(0x00, 0xff, 0xff)),
        15 => Some(rgb(0xff, 0xff, 0xff)),
        16..=231 => Some(xterm_color_cube(index)),
        232..=255 => Some(xterm_grayscale(index)),
        x if x == NamedColor::Foreground as usize => Some(rgb(fr, fg, fb)),
        x if x == NamedColor::Background as usize => Some(rgb(br, bg, bb)),
        x if x == NamedColor::Cursor as usize => Some(rgb(fr, fg, fb)),
        x if x == NamedColor::DimBlack as usize => Some(rgb(0x00, 0x00, 0x00)),
        x if x == NamedColor::DimRed as usize => Some(rgb(0x80, 0x00, 0x00)),
        x if x == NamedColor::DimGreen as usize => Some(rgb(0x00, 0x80, 0x00)),
        x if x == NamedColor::DimYellow as usize => Some(rgb(0x80, 0x80, 0x00)),
        x if x == NamedColor::DimBlue as usize => Some(rgb(0x00, 0x00, 0x80)),
        x if x == NamedColor::DimMagenta as usize => Some(rgb(0x80, 0x00, 0x80)),
        x if x == NamedColor::DimCyan as usize => Some(rgb(0x00, 0x80, 0x80)),
        x if x == NamedColor::DimWhite as usize => Some(rgb(0x80, 0x80, 0x80)),
        x if x == NamedColor::BrightForeground as usize => Some(rgb(fr, fg, fb)),
        x if x == NamedColor::DimForeground as usize => Some(rgb(0x80, 0x80, 0x80)),
        _ => None,
    }
}

fn xterm_color_cube(index: usize) -> Rgb {
    const STEPS: [u8; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let idx = index - 16;
    rgb(STEPS[idx / 36], STEPS[(idx / 6) % 6], STEPS[idx % 6])
}

fn xterm_grayscale(index: usize) -> Rgb {
    let level = 8 + ((index - 232) as u8 * 10);
    rgb(level, level, level)
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Scan raw child output for modifyOtherKeys CSI sequences and update the
/// tracked level. alacritty_terminal 0.26 silently ignores these; we track
/// them so `mirrored_modes()` can mirror to the host. (§4.2.3a.4)
///
/// Patterns: `CSI > 4 ; Nm h/l/m` or `CSI > 4 h/l/m`.
fn scan_modify_other_keys(data: &[u8], level: &mut u8) {
    // Look for ESC [ > 4
    let pat: &[u8] = b"\x1b[>4";
    let mut i = 0;
    while i + pat.len() < data.len() {
        // Find next occurrence of the pattern
        let remaining = &data[i..];
        let Some(rel) = remaining.windows(pat.len()).position(|w| w == pat) else {
            break;
        };
        let start = i + rel;
        let mut j = start + pat.len();
        // Skip optional '; Nm' parameter
        let mut new_level: Option<u8> = None;
        if j < data.len() && data[j] == b';' {
            j += 1;
            let mut val = 0u8;
            while j < data.len() && data[j].is_ascii_digit() {
                val = val.saturating_mul(10).saturating_add(data[j] - b'0');
                j += 1;
            }
            new_level = Some(val);
        }
        // Final byte must be h (0x68), l (0x6c), or m (0x6d)
        if j < data.len() {
            match data[j] {
                b'h' | b'm' => {
                    // Enable: use parsed level, default 1
                    *level = new_level.unwrap_or(1);
                }
                b'l' => {
                    // Disable
                    *level = 0;
                }
                _ => {}
            }
        }
        i = j + 1;
    }
}

/// Minimal base64 encoder for OSC 52 clipboard pass-through.
/// alacritty decodes the child's base64 before firing ClipboardStore;
/// we must re-encode for the wire format to the outer terminal.
fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[((n >> 18) & 0x3f) as usize] as char);
        out.push(TBL[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TBL[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TBL[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn convert_color(
    color: TermColor,
    palette: &alacritty_terminal::term::color::Colors,
    surface: HostSurface,
) -> Color {
    match color {
        TermColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        TermColor::Indexed(index) => {
            // Child-defined OSC 4 slot → concrete RGB.
            if let Some(c) = palette[index as usize] {
                return Color::Rgb(c.r, c.g, c.b);
            }
            // Both dark and light: keep Indexed so the host ANSI palette remaps
            // (omp statusline / git colors match the outer terminal theme).
            Color::Indexed(index)
        }
        TermColor::Named(named) => {
            // Dynamic FG/BG: FG stays surface RGB (matches OSC 10); BG paints as
            // Reset so the outer terminal's transparency / host shade shows —
            // same as omp's `\x1b[49m` default. OSC 11 replies still use
            // `resolve_color_request_rgb` → surface.bg (see tests), so nested
            // omp keeps a correct light/dark signal without a solid page wash.
            if is_dynamic_named(named) {
                return named_color_to_tui(named, surface);
            }
            palette[named]
                .map(|c| Color::Rgb(c.r, c.g, c.b))
                .unwrap_or_else(|| named_color_to_tui(named, surface))
        }
    }
}

fn is_dynamic_named(color: NamedColor) -> bool {
    matches!(
        color,
        NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
    )
}

fn named_color_to_tui(color: NamedColor, surface: HostSurface) -> Color {
    let (fr, fg, fb) = surface.fg;
    match color {
        NamedColor::Black => Color::Indexed(0),
        NamedColor::Red => Color::Indexed(1),
        NamedColor::Green => Color::Indexed(2),
        NamedColor::Yellow => Color::Indexed(3),
        NamedColor::Blue => Color::Indexed(4),
        NamedColor::Magenta => Color::Indexed(5),
        NamedColor::Cyan => Color::Indexed(6),
        NamedColor::White => Color::Indexed(7),
        NamedColor::BrightBlack => Color::Indexed(8),
        NamedColor::BrightRed => Color::Indexed(9),
        NamedColor::BrightGreen => Color::Indexed(10),
        NamedColor::BrightYellow => Color::Indexed(11),
        NamedColor::BrightBlue => Color::Indexed(12),
        NamedColor::BrightMagenta => Color::Indexed(13),
        NamedColor::BrightCyan => Color::Indexed(14),
        NamedColor::BrightWhite => Color::Indexed(15),
        NamedColor::DimBlack => Color::Indexed(0),
        NamedColor::DimRed => Color::Indexed(1),
        NamedColor::DimGreen => Color::Indexed(2),
        NamedColor::DimYellow => Color::Indexed(3),
        NamedColor::DimBlue => Color::Indexed(4),
        NamedColor::DimMagenta => Color::Indexed(5),
        NamedColor::DimCyan => Color::Indexed(6),
        NamedColor::DimWhite => Color::Indexed(7),
        // FG: concrete surface RGB (aligned with OSC 10 answers).
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => {
            Color::Rgb(fr, fg, fb)
        }
        // BG: Reset leaks host (transparency). Do not paint surface.bg RGB here —
        // that made nested omp a solid card vs direct-tmux glass. OSC 11 still
        // reports `surface.bg` via `resolve_color_request_rgb` / `default_palette_rgb`.
        NamedColor::Background => Color::Reset,
        NamedColor::DimForeground => Color::Rgb(0x80, 0x80, 0x80),
    }
}

/// Headless smoke: spawn a short-lived command in a PTY and wait for exit.
pub fn smoke_spawn_echo() -> Result<()> {
    let mut session = PtySession::spawn(
        "echo",
        &["amux-smoke".into()],
        Path::new("/tmp"),
        24,
        80,
        &[],
        false,
        HostSurface::fallback(Appearance::Dark),
    )?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if session.is_exited() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Mark ready artificially not needed — echo exits quickly.
    session.kill_process_group();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark_surface() -> HostSurface {
        HostSurface::fallback(Appearance::Dark)
    }

    fn light_surface() -> HostSurface {
        HostSurface::fallback(Appearance::Light)
    }

    #[test]
    fn smoke_echo_pty() {
        smoke_spawn_echo().expect("echo pty smoke");
    }

    #[test]
    fn snapshot_drops_bold_italic_reversed_keeps_underline() {
        let mut term = TerminalState::new(5, 40, false, light_surface());
        // bold + italic + underline + inverse + mediumGray (omp quote-like)
        let (_replies, _) = term.process(
            b"\x1b[1;3;4;7m\x1b[38;2;108;108;108mQUOTE\x1b[0m\n",
        );
        let snap = term.snapshot();
        let cells: Vec<_> = snap
            .cells
            .iter()
            .filter(|c| matches!(c.symbol.as_str(), "Q" | "U" | "O" | "T" | "E"))
            .collect();
        assert!(!cells.is_empty(), "expected QUOTE cells");
        for c in cells {
            assert!(
                !c.modifier
                    .intersects(Modifier::BOLD | Modifier::ITALIC | Modifier::REVERSED),
                "attr must not be forwarded: {:?}",
                c.modifier
            );
            assert!(
                c.modifier.contains(Modifier::UNDERLINED),
                "underline should still forward: {:?}",
                c.modifier
            );
            assert_eq!(c.fg, Color::Rgb(108, 108, 108));
        }
    }


    #[test]
    fn color_query_fallback_uses_host_surface() {
        let empty = alacritty_terminal::term::color::Colors::default();
        let host = HostSurface::from_bg(0x1e, 0x1e, 0x2e);
        assert_eq!(
            resolve_color_request_rgb(NamedColor::Background as usize, &empty, host),
            rgb(0x1e, 0x1e, 0x2e)
        );
        assert_eq!(
            resolve_color_request_rgb(NamedColor::Foreground as usize, &empty, host),
            rgb(host.fg.0, host.fg.1, host.fg.2)
        );
        assert_eq!(
            resolve_color_request_rgb(1, &empty, dark_surface()),
            rgb(0xcd, 0x00, 0x00)
        );
        assert_eq!(
            resolve_color_request_rgb(238, &empty, dark_surface()),
            xterm_grayscale(238)
        );
    }

    #[test]
    fn osc_background_query_follows_host_surface() {
        let host = HostSurface::from_bg(0x1e, 0x1e, 0x2e);
        let mut dark = TerminalState::new(3, 16, false, host);
        let (replies, _) = dark.process(b"\x1b]11;?\x07");
        let response = String::from_utf8_lossy(&replies);
        assert!(
            response.contains("\x1b]11;rgb:1e1e/1e1e/2e2e"),
            "expected host bg reply, got: {response:?}"
        );

        let mut light = TerminalState::new(3, 16, false, light_surface());
        let (replies, _) = light.process(b"\x1b]11;?\x07");
        let response = String::from_utf8_lossy(&replies);
        assert!(
            response.contains("\x1b]11;rgb:f8f8/f8f8/f8f8"),
            "expected light theme bg reply, got: {response:?}"
        );
    }

    #[test]
    fn scroll_display_lines_moves_into_history() {
        let mut terminal = TerminalState::new(5, 20, false, dark_surface());
        let mut bump = Vec::new();
        for i in 0..40 {
            bump.extend_from_slice(format!("line-{i}\r\n").as_bytes());
        }
        let _ = terminal.process(&bump);
        assert_eq!(terminal.term.grid().display_offset(), 0);
        assert!(terminal.scroll_display_lines(3));
        assert_eq!(terminal.term.grid().display_offset(), 3);
        assert!(terminal.scroll_display_lines(-3));
        assert_eq!(terminal.term.grid().display_offset(), 0);
        let _ = terminal.process(b"\x1b[?1049h");
        assert!(!terminal.scroll_display_lines(5));
    }

    #[test]
    fn convert_named_background_resets_to_leak_host_transparency() {
        let empty = alacritty_terminal::term::color::Colors::default();
        let host = HostSurface::from_bg(0x1e, 0x1e, 0x2e);
        assert_eq!(
            convert_color(TermColor::Named(NamedColor::Red), &empty, host),
            Color::Indexed(1)
        );
        // Paint path: default bg must be Reset so outer terminal transparency shows.
        assert_eq!(
            convert_color(TermColor::Named(NamedColor::Background), &empty, host),
            Color::Reset
        );
        assert_eq!(
            convert_color(TermColor::Named(NamedColor::Foreground), &empty, host),
            Color::Rgb(host.fg.0, host.fg.1, host.fg.2)
        );
        let light = light_surface();
        assert_eq!(
            convert_color(TermColor::Named(NamedColor::Background), &empty, light),
            Color::Reset
        );
        // Explicit RGB / Indexed chips (omp userMessageBg, tool cards) stay concrete.
        assert_eq!(
            convert_color(TermColor::Spec(rgb(0xe8, 0xe8, 0xe8)), &empty, light),
            Color::Rgb(0xe8, 0xe8, 0xe8)
        );
        assert_eq!(
            convert_color(TermColor::Indexed(1), &empty, dark_surface()),
            Color::Indexed(1)
        );
        assert_eq!(
            convert_color(TermColor::Indexed(1), &empty, light_surface()),
            Color::Indexed(1)
        );
    }

    #[test]
    fn osc_bg_query_still_reports_host_surface_rgb() {
        // Dual-palette guard: paint uses Reset, but child OSC 11 must still see
        // the probed surface so omp picks light/dark correctly (DCS-wrap helps
        // the host probe; this keeps the nested answer honest).
        let empty = alacritty_terminal::term::color::Colors::default();
        let light = light_surface();
        assert_eq!(
            resolve_color_request_rgb(NamedColor::Background as usize, &empty, light),
            rgb(light.bg.0, light.bg.1, light.bg.2)
        );
        assert_ne!(
            convert_color(TermColor::Named(NamedColor::Background), &empty, light),
            Color::Rgb(light.bg.0, light.bg.1, light.bg.2)
        );
    }
}
