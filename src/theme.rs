//! Amux chrome + shortcut hint colors.
//!
//! Keys & brackets: bright cyan (truecolor, not ANSI remap).
//! Descriptions: dux bright `hint_desc_fg` (#a0a0a0).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Bright cyan — dux `ansi_cyan` / `#00FFFF`.
const BRIGHT_CYAN: Color = Color::Rgb(0, 255, 255);
/// dux `hint_desc_fg`.
const DESC_BRIGHT: Color = Color::Rgb(160, 160, 160);
const APP_BG: Color = Color::Rgb(20, 20, 20);
/// Modal panel fill. Prefer 256-color index so WebSSH / `TERM=xterm`
/// still separates from app_bg when truecolor RGB is ignored.
const OVERLAY_BG: Color = Color::Indexed(60); // #5f5f87 slate
const OVERLAY_SCRIM: Color = Color::Indexed(232); // near-black veil
const BORDER_NORMAL: Color = Color::Rgb(80, 80, 80);
const TITLE_MUTED: Color = Color::Rgb(140, 140, 140);

#[derive(Debug, Clone)]
pub struct Theme {
    pub app_bg: Color,
    pub text_fg: Color,
    pub border_focused: Color,
    pub border_normal: Color,
    pub title_focused: Color,
    pub title_normal: Color,
    pub selection_fg: Color,
    pub selection_bg: Color,
    pub session_active: Color,
    pub session_detached: Color,
    pub session_exited: Color,
    pub hint_key_fg: Color,
    pub hint_bracket_fg: Color,
    pub hint_key_bg: Color,
    pub hint_desc_fg: Color,
    pub hint_dim_key_fg: Color,
    pub hint_dim_bracket_fg: Color,
    pub hint_dim_desc_fg: Color,
    pub hint_bar_bg: Color,
    pub overlay_border: Color,
    pub overlay_bg: Color,
    pub overlay_dim_bg: Color,
    pub overlay_dim_fg: Color,
    pub input_cursor_fg: Color,
    pub input_cursor_bg: Color,
    pub input_label_fg: Color,
    pub status_info_fg: Color,
    pub status_info_bg: Color,
    pub project_icon: Color,
    /// Powerline status segments (tmux/vim style).
    pub status_mode_agent_bg: Color,
    pub status_mode_shell_bg: Color,
    pub status_mode_modal_bg: Color,
    pub status_mode_fg: Color,
    pub status_seg_a_bg: Color,
    pub status_seg_b_bg: Color,
    pub status_seg_fg: Color,
    pub status_msg_bg: Color,
    pub status_msg_fg: Color,
    /// Right-aligned workspace chip (must contrast with app_bg).
    pub status_ws_bg: Color,
    pub status_ws_fg: Color,
    /// omp-like transcript preview (JSONL, non-running sessions).
    pub transcript_user_bg: Color,
    pub transcript_user_fg: Color,
    pub transcript_assistant_fg: Color,
    pub transcript_tool_fg: Color,
    pub transcript_thinking_fg: Color,
    pub transcript_meta_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            app_bg: APP_BG,
            text_fg: Color::Rgb(255, 255, 255),
            border_focused: BRIGHT_CYAN,
            border_normal: BORDER_NORMAL,
            title_focused: BRIGHT_CYAN,
            title_normal: TITLE_MUTED,
            selection_fg: Color::Rgb(0, 0, 0),
            selection_bg: BRIGHT_CYAN,
            session_active: Color::Rgb(210, 210, 210),
            session_detached: Color::Rgb(255, 200, 60),
            session_exited: Color::Rgb(100, 100, 100),
            hint_key_fg: BRIGHT_CYAN,
            hint_bracket_fg: BRIGHT_CYAN,
            hint_key_bg: Color::Rgb(35, 35, 35),
            hint_desc_fg: DESC_BRIGHT,
            hint_dim_key_fg: BRIGHT_CYAN,
            hint_dim_bracket_fg: BRIGHT_CYAN,
            hint_dim_desc_fg: DESC_BRIGHT,
            hint_bar_bg: Color::Rgb(25, 25, 25),
            overlay_border: BRIGHT_CYAN,
            overlay_bg: OVERLAY_BG,
            overlay_dim_bg: OVERLAY_SCRIM,
            overlay_dim_fg: Color::Rgb(128, 128, 128),
            input_cursor_fg: Color::Rgb(0, 0, 0),
            input_cursor_bg: Color::Rgb(255, 255, 255),
            input_label_fg: Color::Rgb(255, 255, 255),
            status_info_fg: Color::Rgb(100, 100, 100),
            status_info_bg: Color::Rgb(25, 25, 25),
            project_icon: Color::Rgb(100, 149, 237),
            status_mode_agent_bg: Color::Rgb(0, 175, 175),
            status_mode_shell_bg: Color::Rgb(215, 175, 0),
            status_mode_modal_bg: Color::Rgb(175, 135, 255),
            status_mode_fg: Color::Rgb(0, 0, 0),
            status_seg_a_bg: Color::Rgb(60, 60, 60),
            status_seg_b_bg: Color::Rgb(80, 100, 140),
            status_seg_fg: Color::Rgb(230, 230, 230),
            status_msg_bg: Color::Rgb(40, 40, 40),
            status_msg_fg: Color::Rgb(180, 180, 180),
            // Match tmux-ish current-window blue (visible on dark fill).
            status_ws_bg: Color::Rgb(50, 100, 180),
            status_ws_fg: Color::Rgb(255, 255, 255),
            // Soft blue-gray bubble ≈ omp userMessageBg on dark themes.
            transcript_user_bg: Color::Rgb(40, 55, 75),
            transcript_user_fg: Color::Rgb(230, 235, 245),
            transcript_assistant_fg: Color::Rgb(220, 220, 220),
            transcript_tool_fg: Color::Rgb(120, 180, 200),
            transcript_thinking_fg: Color::Rgb(140, 140, 160),
            transcript_meta_fg: Color::Rgb(120, 120, 120),
        }
    }
}

impl Theme {
    pub fn selection_style(&self) -> Style {
        Style::default()
            .fg(self.selection_fg)
            .bg(self.selection_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn key_badge<'a>(&self, key: &'a str) -> Vec<Span<'a>> {
        let bg = self.app_bg;
        vec![
            Span::styled("<", Style::default().fg(self.hint_bracket_fg).bg(bg)),
            Span::styled(
                key,
                Style::default()
                    .fg(self.hint_key_fg)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(">", Style::default().fg(self.hint_bracket_fg).bg(bg)),
        ]
    }

    /// Owned key badge for composing dynamic footer lines.
    pub fn key_badge_owned(&self, key: &str) -> Vec<Span<'static>> {
        let bg = self.overlay_bg;
        vec![
            Span::styled("<", Style::default().fg(self.hint_bracket_fg).bg(bg)),
            Span::styled(
                key.to_string(),
                Style::default()
                    .fg(self.hint_key_fg)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(">", Style::default().fg(self.hint_bracket_fg).bg(bg)),
        ]
    }

    pub fn dim_key_badge<'a>(&self, key: &'a str) -> Vec<Span<'a>> {
        self.key_badge(key)
    }

    pub fn desc_span<'a>(&self, text: &'a str) -> Span<'a> {
        Span::styled(text, Style::default().fg(self.hint_desc_fg).bg(self.app_bg))
    }

    pub fn dim_desc_span<'a>(&self, text: &'a str) -> Span<'a> {
        self.desc_span(text)
    }

    pub fn hint_pair<'a>(&self, key: &'a str, desc: &'a str) -> Vec<Span<'a>> {
        let mut spans = self.key_badge(key);
        spans.push(self.desc_span(" "));
        spans.push(self.desc_span(desc));
        spans.push(self.desc_span("  "));
        spans
    }

    pub fn help_row<'a>(&self, key: &'a str, desc: &'a str) -> Line<'a> {
        let mut spans = self.key_badge(key);
        spans.push(self.desc_span("  "));
        spans.push(self.desc_span(desc));
        Line::from(spans)
    }
}
