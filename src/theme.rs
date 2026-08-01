//! Amux chrome + shortcut hint colors.
//!
//! Keys & brackets: bright cyan (truecolor, not ANSI remap).
//! Descriptions: dux bright `hint_desc_fg` (#a0a0a0).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::appearance::Appearance;

/// Bright cyan — dux `ansi_cyan` / `#00FFFF`.
const BRIGHT_CYAN: Color = Color::Rgb(0, 255, 255);
/// dux `hint_desc_fg`.
const DESC_BRIGHT: Color = Color::Rgb(160, 160, 160);
/// Modal panel fill. Prefer 256-color index so WebSSH / `TERM=xterm`
/// still separates from app_bg when truecolor RGB is ignored.
const OVERLAY_BG: Color = Color::Indexed(60); // #5f5f87 slate
const OVERLAY_SCRIM: Color = Color::Indexed(232); // near-black veil
const BORDER_NORMAL: Color = Color::Rgb(80, 80, 80);
const TITLE_MUTED: Color = Color::Rgb(140, 140, 140);

const LIGHT_TEXT: Color = Color::Rgb(30, 30, 30);
const LIGHT_BORDER: Color = Color::Rgb(176, 176, 176); // lightGray
const LIGHT_TITLE_MUTED: Color = Color::Rgb(108, 108, 108); // mediumGray
/// omp light.json teal — keep for soft transcript / md accents.
const LIGHT_TEAL: Color = Color::Rgb(90, 128, 128);
/// Stronger chrome accent for selection / focused borders on white host bg.
const LIGHT_ACCENT: Color = Color::Rgb(0, 140, 150);
const LIGHT_SELECTION_BG: Color = Color::Rgb(0, 165, 175);
const LIGHT_SELECTION_FG: Color = Color::Rgb(255, 255, 255);
const LIGHT_OVERLAY_BG: Color = Color::Indexed(254); // near-white panel
const LIGHT_OVERLAY_SCRIM: Color = Color::Indexed(252); // light veil
/// omp transcript symbols (theme.ts symbol preset).
const OMP_TREE_BRANCH: &str = "├─";
const OMP_TREE_LAST: &str = "└─";
const OMP_TREE_VERTICAL: &str = "│";
const OMP_STATUS_OK: &str = "✔";
const OMP_STATUS_ERR: &str = "✘";
const OMP_STATUS_WARN: &str = "⚠";
const OMP_STATUS_PENDING: &str = "⏳";
const OMP_BULLET: &str = "•";
const OMP_SEP_DOT: &str = " · ";
/// omp rounded box glyphs (renderOutputBlock frame).
const OMP_BOX_TL: &str = "╭";
const OMP_BOX_TR: &str = "╮";
const OMP_BOX_BL: &str = "╰";
const OMP_BOX_BR: &str = "╯";
const OMP_BOX_V: &str = "│";
const OMP_BOX_H: &str = "─";

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
    /// Confirm dialogs (Indexed slate — visible under WebSSH).
    pub overlay_bg: Color,
    pub overlay_dim_bg: Color,
    pub overlay_dim_fg: Color,
    /// Help panel — dark like dux (`app_bg`), not the confirm slate.
    pub help_panel_bg: Color,
    pub help_banner_fg: Color,
    pub help_banner_bg: Color,
    pub help_body_fg: Color,
    pub help_section_fg: Color,
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
    /// omp semantic accent (amber on dark).
    pub accent: Color,
    pub border_muted: Color,
    pub border_accent: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub muted: Color,
    pub dim: Color,
    /// omp markdown semantic colors.
    pub md_heading: Color,
    pub md_link: Color,
    pub md_code: Color,
    pub md_code_block: Color,
    pub md_code_block_border: Color,
    pub md_quote: Color,
    pub md_quote_border: Color,
    pub md_hr: Color,
    pub md_list_bullet: Color,
    /// omp tool-card state backgrounds.
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    pub tool_pending_bg: Color,
    pub tool_output: Color,
    /// omp custom/compaction block colors.
    pub custom_message_bg: Color,
    pub custom_message_label: Color,
    /// omp execution mode colors.
    pub bash_mode: Color,
    pub python_mode: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            // Large fills use Reset so chrome matches the host dark palette
            // instead of a hard #141414 box around omp.
            app_bg: Color::Reset,
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
            hint_key_bg: Color::Indexed(236),
            hint_desc_fg: DESC_BRIGHT,
            hint_dim_key_fg: BRIGHT_CYAN,
            hint_dim_bracket_fg: BRIGHT_CYAN,
            hint_dim_desc_fg: DESC_BRIGHT,
            hint_bar_bg: Color::Reset,
            overlay_border: BRIGHT_CYAN,
            overlay_bg: OVERLAY_BG,
            overlay_dim_bg: OVERLAY_SCRIM,
            overlay_dim_fg: Color::Rgb(128, 128, 128),
            help_panel_bg: Color::Indexed(234), // #1c1c1c — dux-like dark panel
            help_banner_fg: Color::Rgb(20, 20, 20),
            help_banner_bg: BRIGHT_CYAN,
            help_body_fg: Color::Rgb(180, 180, 180),
            help_section_fg: BRIGHT_CYAN,
            input_cursor_fg: Color::Rgb(0, 0, 0),
            input_cursor_bg: Color::Rgb(255, 255, 255),
            input_label_fg: Color::Rgb(255, 255, 255),
            status_info_fg: Color::Rgb(100, 100, 100),
            status_info_bg: Color::Reset,
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
            accent: Color::Rgb(0xfe, 0xbc, 0x38),
            border_muted: Color::Rgb(0x3d, 0x42, 0x4a),
            border_accent: Color::Rgb(0x00, 0x88, 0xfa),
            success: Color::Rgb(0x89, 0xd2, 0x81),
            error: Color::Rgb(0xfc, 0x3a, 0x4b),
            warning: Color::Rgb(0xe4, 0xc0, 0x0f),
            muted: Color::Rgb(0x77, 0x7d, 0x88),
            dim: Color::Rgb(0x5f, 0x66, 0x73),
            md_heading: Color::Rgb(0xfe, 0xbc, 0x38),
            md_link: Color::Rgb(0x00, 0x88, 0xfa),
            md_code: Color::Rgb(0xe5, 0xc1, 0xff),
            md_code_block: Color::Rgb(0x9c, 0xdc, 0xfe),
            md_code_block_border: Color::Rgb(0x3d, 0x42, 0x4a),
            md_quote: Color::Rgb(0x77, 0x7d, 0x88),
            md_quote_border: Color::Rgb(0x3d, 0x42, 0x4a),
            md_hr: Color::Rgb(0x3d, 0x42, 0x4a),
            md_list_bullet: Color::Rgb(0xfe, 0xbc, 0x38),
            tool_success_bg: Color::Rgb(0x16, 0x1a, 0x1f),
            tool_error_bg: Color::Rgb(0x29, 0x1d, 0x1d),
            tool_pending_bg: Color::Rgb(0x1d, 0x21, 0x29),
            tool_output: Color::Rgb(0x77, 0x7d, 0x88),
            custom_message_bg: Color::Rgb(0x2a, 0x25, 0x30),
            custom_message_label: Color::Rgb(0xb2, 0x81, 0xd6),
            bash_mode: Color::Rgb(0x00, 0x88, 0xfa),
            python_mode: Color::Rgb(0xe4, 0xc0, 0x0f),
        }
    }

    /// Light chrome: vivid selection/borders (readable on white host);
    /// transcript soft fills stay close to omp `light.json`.
    pub fn light() -> Self {
        Self {
            // Match dark: leak host light background instead of a hard #f8f8f8 box.
            app_bg: Color::Reset,
            text_fg: LIGHT_TEXT,
            border_focused: LIGHT_ACCENT,
            border_normal: LIGHT_BORDER,
            title_focused: LIGHT_ACCENT,
            title_normal: LIGHT_TITLE_MUTED,
            selection_fg: LIGHT_SELECTION_FG,
            selection_bg: LIGHT_SELECTION_BG,
            session_active: Color::Rgb(40, 40, 40),
            session_detached: Color::Rgb(154, 115, 38), // yellow
            session_exited: Color::Rgb(118, 118, 118), // dimGray
            hint_key_fg: LIGHT_ACCENT,
            hint_bracket_fg: LIGHT_ACCENT,
            hint_key_bg: Color::Indexed(254),
            hint_desc_fg: LIGHT_TITLE_MUTED,
            hint_dim_key_fg: LIGHT_ACCENT,
            hint_dim_bracket_fg: LIGHT_ACCENT,
            hint_dim_desc_fg: LIGHT_TITLE_MUTED,
            hint_bar_bg: Color::Reset,
            overlay_border: LIGHT_ACCENT,
            overlay_bg: LIGHT_OVERLAY_BG,
            overlay_dim_bg: LIGHT_OVERLAY_SCRIM,
            overlay_dim_fg: Color::Rgb(118, 118, 118),
            help_panel_bg: Color::Indexed(255), // white panel
            help_banner_fg: Color::Rgb(255, 255, 255),
            help_banner_bg: LIGHT_ACCENT,
            help_body_fg: Color::Rgb(60, 60, 60),
            help_section_fg: LIGHT_ACCENT,
            input_cursor_fg: Color::Rgb(255, 255, 255),
            input_cursor_bg: LIGHT_TEXT,
            input_label_fg: LIGHT_TEXT,
            status_info_fg: LIGHT_TITLE_MUTED,
            status_info_bg: Color::Reset,
            project_icon: Color::Rgb(84, 125, 167),   // blue
            // Powerline mode pills — saturated so NAV/AGENT read at a glance.
            status_mode_agent_bg: Color::Rgb(0, 145, 155),
            status_mode_shell_bg: Color::Rgb(200, 110, 0),
            status_mode_modal_bg: Color::Rgb(150, 90, 170),
            status_mode_fg: Color::Rgb(255, 255, 255),
            status_seg_a_bg: Color::Rgb(220, 220, 220),
            // Session / path chips: deep teal-blue + white (was dark-on-dark).
            status_seg_b_bg: Color::Rgb(0, 110, 145),
            status_seg_fg: Color::Rgb(255, 255, 255),
            status_msg_bg: Color::Rgb(235, 238, 242),
            status_msg_fg: Color::Rgb(50, 55, 65),
            status_ws_bg: Color::Rgb(0, 110, 145),
            status_ws_fg: Color::Rgb(255, 255, 255),
            // omp light userMsgBg / thinking / toolOutput
            transcript_user_bg: Color::Rgb(232, 232, 232), // #e8e8e8
            transcript_user_fg: LIGHT_TEXT,
            transcript_assistant_fg: LIGHT_TEXT,
            transcript_tool_fg: Color::Rgb(108, 108, 108), // mediumGray toolOutput
            transcript_thinking_fg: Color::Rgb(108, 108, 108), // thinkingText
            transcript_meta_fg: Color::Rgb(118, 118, 118), // dimGray
            accent: LIGHT_ACCENT,
            border_muted: LIGHT_BORDER,
            border_accent: LIGHT_ACCENT,
            success: Color::Rgb(0x50, 0x8c, 0x50),
            error: Color::Rgb(0xc8, 0x3c, 0x3c),
            warning: Color::Rgb(0xb4, 0x8c, 0x00),
            muted: LIGHT_TITLE_MUTED,
            dim: Color::Rgb(0x8c, 0x8c, 0x8c),
            md_heading: LIGHT_ACCENT,
            md_link: Color::Rgb(0x00, 0x5f, 0x87),
            md_code: Color::Rgb(0x78, 0x3c, 0x8c),
            md_code_block: LIGHT_TEXT,
            md_code_block_border: LIGHT_BORDER,
            md_quote: LIGHT_TITLE_MUTED,
            md_quote_border: LIGHT_BORDER,
            md_hr: LIGHT_BORDER,
            md_list_bullet: LIGHT_TEAL,
            tool_success_bg: Color::Rgb(0xf0, 0xf0, 0xf0),
            tool_error_bg: Color::Rgb(0xf0, 0xdc, 0xdc),
            tool_pending_bg: Color::Rgb(0xeb, 0xeb, 0xeb),
            tool_output: LIGHT_TITLE_MUTED,
            custom_message_bg: Color::Rgb(0xee, 0xeb, 0xf2),
            custom_message_label: Color::Rgb(0x8c, 0x5a, 0xaa),
            bash_mode: Color::Rgb(0x00, 0x5f, 0x87),
            python_mode: Color::Rgb(0xb4, 0x8c, 0x00),
        }
    }

    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_theme_leaks_host_app_bg_like_dark() {
        assert_eq!(Theme::light().app_bg, Color::Reset);
        assert_eq!(Theme::dark().app_bg, Color::Reset);
        assert_ne!(
            Theme::dark().transcript_user_bg,
            Theme::light().transcript_user_bg
        );
    }

    #[test]
    fn light_selection_and_status_are_high_contrast() {
        let t = Theme::light();
        assert_eq!(t.selection_bg, LIGHT_SELECTION_BG);
        assert_eq!(t.selection_fg, LIGHT_SELECTION_FG);
        assert_eq!(t.border_focused, LIGHT_ACCENT);
        // Colored status chips use light fg (not LIGHT_TEXT on dark teal).
        assert_eq!(t.status_seg_fg, Color::Rgb(255, 255, 255));
        assert_eq!(t.status_mode_fg, Color::Rgb(255, 255, 255));
        assert_ne!(t.status_mode_agent_bg, t.status_mode_shell_bg);
    }
}
