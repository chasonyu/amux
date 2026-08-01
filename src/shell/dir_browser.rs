//! Add-workspace directory browser — layout & keys aligned with dux BrowseProjects.

use std::path::{Path, PathBuf};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Widget,
};
use ratatui::Frame;

use crate::theme::Theme;

use super::text_input::LineInput;

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub path: PathBuf,
    pub label: String,
    pub is_git_repo: bool,
}

#[derive(Debug, Clone)]
pub struct DirBrowser {
    pub cwd: PathBuf,
    pub entries: Vec<DirEntry>,
    pub selected: usize,
    pub filter: LineInput,
    pub searching: bool,
    pub editing_path: bool,
    pub path_input: LineInput,
    pub tab_completions: Vec<String>,
    pub tab_index: usize,
    pub error: Option<String>,
}

/// Result of handling one key sequence inside the browser.
#[derive(Debug)]
pub enum BrowserResult {
    /// Keep browser open (possibly updated).
    Continue(DirBrowser),
    /// Close without adding.
    Close,
    /// Add this path as a workspace; on failure restore `browser` with error.
    Add {
        path: PathBuf,
        browser: DirBrowser,
    },
}

impl DirBrowser {
    pub fn open() -> Self {
        let cwd = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            entries: browser_entries(&cwd),
            cwd,
            selected: 0,
            filter: LineInput::new(),
            searching: false,
            editing_path: false,
            path_input: LineInput::new(),
            tab_completions: Vec::new(),
            tab_index: 0,
            error: None,
        }
    }

    pub fn with_error(mut self, err: String) -> Self {
        self.error = Some(err);
        self
    }

    fn visible(&self) -> Vec<&DirEntry> {
        if self.filter.is_empty() {
            self.entries.iter().collect()
        } else {
            let needle = self.filter.text.to_lowercase();
            self.entries
                .iter()
                .filter(|e| e.label.to_lowercase().contains(&needle))
                .collect()
        }
    }

    fn refresh_completions(&mut self) {
        self.tab_completions = path_completion_candidates(&self.path_input.text);
        self.tab_index = 0;
    }

    pub fn handle_seq(mut self, seq: &[u8]) -> BrowserResult {
        self.error = None;

        if self.editing_path {
            return self.handle_path_editor(seq);
        }

        if self.searching {
            return self.handle_searching(seq);
        }

        // Normal browse mode (dux CloseOverlay / OpenEntry / AddCurrentDir / …)
        match seq {
            b"\x1b" => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.selected = 0;
                    BrowserResult::Continue(self)
                } else {
                    BrowserResult::Close
                }
            }
            b"j" | b"\x1b[B" => {
                let len = self.visible().len();
                if len > 0 && self.selected + 1 < len {
                    self.selected += 1;
                }
                BrowserResult::Continue(self)
            }
            b"k" | b"\x1b[A" => {
                self.selected = self.selected.saturating_sub(1);
                BrowserResult::Continue(self)
            }
            b"/" => {
                self.filter.move_end();
                self.searching = true;
                BrowserResult::Continue(self)
            }
            b"g" => {
                self.editing_path = true;
                let mut p = self.cwd.to_string_lossy().into_owned();
                if !p.ends_with('/') {
                    p.push('/');
                }
                self.path_input.set_text(p);
                self.refresh_completions();
                BrowserResult::Continue(self)
            }
            b"o" | b"O" => {
                let path = self.cwd.clone();
                BrowserResult::Add {
                    path,
                    browser: self,
                }
            }
            b"\r" | b"\n" | b"l" | b"\x1b[C" => self.open_selected(),
            _ => BrowserResult::Continue(self),
        }
    }

    fn handle_searching(mut self, seq: &[u8]) -> BrowserResult {
        match seq {
            b"\x1b" => {
                // Exit search mode; keep filter text (dux CloseOverlay while searching).
                self.searching = false;
                BrowserResult::Continue(self)
            }
            b"\r" | b"\n" => {
                self.searching = false;
                BrowserResult::Continue(self)
            }
            // Non-character nav still moves the list while searching (dux binding lookup).
            b"\x1b[B" => {
                let len = self.visible().len();
                if len > 0 && self.selected + 1 < len {
                    self.selected += 1;
                }
                BrowserResult::Continue(self)
            }
            b"\x1b[A" => {
                self.selected = self.selected.saturating_sub(1);
                BrowserResult::Continue(self)
            }
            _ => {
                // Plain chars (incl. j/k) edit the filter.
                if self.filter.handle_seq(seq) {
                    self.selected = 0;
                }
                BrowserResult::Continue(self)
            }
        }
    }

    fn handle_path_editor(mut self, seq: &[u8]) -> BrowserResult {
        match seq {
            b"\x1b" | b"\x07" => {
                // Esc or Ctrl+G — back to browse
                self.editing_path = false;
                self.path_input.clear();
                self.tab_completions.clear();
                self.tab_index = 0;
                BrowserResult::Continue(self)
            }
            b"\t" => {
                if self.tab_completions.is_empty() {
                    self.refresh_completions();
                }
                if let Some(completion) = self.tab_completions.get(self.tab_index).cloned() {
                    self.path_input.set_text(completion);
                    self.refresh_completions();
                }
                BrowserResult::Continue(self)
            }
            b"\x1b[Z" => {
                // BackTab
                if self.tab_completions.is_empty() {
                    self.refresh_completions();
                } else if self.tab_index == 0 {
                    self.tab_index = self.tab_completions.len().saturating_sub(1);
                } else {
                    self.tab_index -= 1;
                }
                BrowserResult::Continue(self)
            }
            b"\x1b[A" => {
                if self.tab_completions.is_empty() {
                    self.refresh_completions();
                } else if self.tab_index == 0 {
                    self.tab_index = self.tab_completions.len().saturating_sub(1);
                } else {
                    self.tab_index -= 1;
                }
                BrowserResult::Continue(self)
            }
            b"\x1b[B" => {
                if self.tab_completions.is_empty() {
                    self.refresh_completions();
                } else if !self.tab_completions.is_empty() {
                    self.tab_index = (self.tab_index + 1) % self.tab_completions.len();
                }
                BrowserResult::Continue(self)
            }
            b"\r" | b"\n" => {
                let path = self.path_input.text.trim().to_string();
                if path.is_empty() {
                    BrowserResult::Continue(self)
                } else {
                    BrowserResult::Add {
                        path: PathBuf::from(path),
                        browser: self,
                    }
                }
            }
            _ => {
                if self.path_input.handle_seq(seq) {
                    self.refresh_completions();
                }
                BrowserResult::Continue(self)
            }
        }
    }

    fn open_selected(mut self) -> BrowserResult {
        let visible: Vec<DirEntry> = self.visible().into_iter().cloned().collect();
        let Some(entry) = visible.get(self.selected).cloned() else {
            return BrowserResult::Continue(self);
        };
        self.cwd = entry.path;
        self.entries = browser_entries(&self.cwd);
        self.selected = 0;
        self.filter.clear();
        self.searching = false;
        BrowserResult::Continue(self)
    }
}

pub fn browser_entries(dir: &Path) -> Vec<DirEntry> {
    let mut entries = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let is_git_repo = path.join(".git").exists();
            let label = if is_git_repo {
                name
            } else {
                format!("{name}/")
            };
            Some(DirEntry {
                path,
                label,
                is_git_repo,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        b.is_git_repo
            .cmp(&a.is_git_repo)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    if let Some(parent) = dir.parent() {
        entries.insert(
            0,
            DirEntry {
                path: parent.to_path_buf(),
                label: "../".into(),
                is_git_repo: false,
            },
        );
    }
    entries
}

fn path_completion_candidates(input: &str) -> Vec<String> {
    let input_path = PathBuf::from(input);
    let (search_dir, prefix) = if input_path.is_dir() && input.ends_with('/') {
        (input_path, String::new())
    } else {
        let parent = input_path
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        let file_name = input_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        (parent, file_name)
    };
    let Ok(read) = std::fs::read_dir(&search_dir) else {
        return Vec::new();
    };
    let prefix_lower = prefix.to_lowercase();
    let mut candidates: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            !name.starts_with('.') && name.starts_with(&prefix_lower)
        })
        .map(|e| {
            let mut full = search_dir.join(e.file_name()).to_string_lossy().into_owned();
            full.push('/');
            full
        })
        .collect();
    candidates.sort();
    candidates
}

fn path_completion_display_label(completion: &str) -> String {
    let trimmed = completion.trim_end_matches('/');
    let Some(folder) = Path::new(trimmed)
        .file_name()
        .and_then(|part| part.to_str())
    else {
        return completion.to_string();
    };
    format!(".../{folder}/")
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area)[1];
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical)[1]
}

fn render_cursor_input<'a>(
    prefix: &'a str,
    text: &'a str,
    cursor: usize,
    theme: &Theme,
) -> Line<'a> {
    let cursor = cursor.min(text.len());
    if cursor < text.len() {
        let (before, after) = text.split_at(cursor);
        let ch = after.chars().next().unwrap();
        let rest = &after[ch.len_utf8()..];
        Line::from(vec![
            Span::raw(prefix),
            Span::raw(before),
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(theme.input_cursor_fg)
                    .bg(theme.input_cursor_bg),
            ),
            Span::raw(rest),
        ])
    } else {
        Line::from(vec![
            Span::raw(format!("{prefix}{text}")),
            Span::styled(
                " ",
                Style::default()
                    .fg(theme.input_cursor_fg)
                    .bg(theme.input_cursor_bg),
            ),
        ])
    }
}

fn footer_hint(theme: &Theme, key: &str, desc: &str) -> Vec<Span<'static>> {
    let mut spans = theme.key_badge_owned(key);
    spans.push(Span::styled(
        format!(" {desc}  "),
        Style::default().fg(theme.hint_desc_fg),
    ));
    spans
}

fn themed_block<'a>(theme: &Theme, title: &'a str) -> Block<'a> {
    Block::default()
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.input_label_fg)
                .add_modifier(Modifier::BOLD),
        )))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.overlay_border))
        .style(Style::default().bg(theme.overlay_bg))
}

pub fn render_dim_overlay(f: &mut Frame, area: Rect, theme: &Theme, footer_h: u16) {
    let dim_h = area.height.saturating_sub(footer_h);
    let buf = f.buffer_mut();
    for y in area.y..area.y + dim_h {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            cell.set_fg(theme.overlay_dim_fg);
            cell.set_bg(theme.overlay_dim_bg);
        }
    }
}

pub fn draw_dir_browser(f: &mut Frame, area: Rect, theme: &Theme, browser: &DirBrowser) {
    // Status strip is 2 rows (hints + status) — keep undimmed like dux.
    render_dim_overlay(f, area, theme, 2);
    let popup = centered_rect(72, 70, area);
    f.render_widget(Clear, popup);

    let visible = browser.visible();
    let show_top = browser.searching || !browser.filter.is_empty() || browser.editing_path;

    let items: Vec<ListItem> = if browser.editing_path {
        if browser.tab_completions.is_empty() {
            vec![ListItem::new("No matching directories.")]
        } else {
            browser
                .tab_completions
                .iter()
                .map(|c| {
                    ListItem::new(Line::from(Span::styled(
                        path_completion_display_label(c),
                        Style::default().fg(theme.text_fg),
                    )))
                })
                .collect()
        }
    } else if visible.is_empty() {
        vec![ListItem::new(if browser.filter.is_empty() {
            "No child directories here."
        } else {
            "No matching entries."
        })]
    } else {
        let last = visible.len() - 1;
        visible
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let prefix = if entry.label == "../" {
                    ""
                } else if i == last {
                    "└── "
                } else {
                    "├── "
                };
                ListItem::new(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(theme.hint_desc_fg)),
                    Span::styled(entry.label.clone(), Style::default().fg(theme.text_fg)),
                ]))
            })
            .collect()
    };

    let item_count = if browser.editing_path {
        browser.tab_completions.len()
    } else {
        visible.len()
    };
    let selected_index = if browser.editing_path {
        browser.tab_index
    } else {
        browser.selected
    };
    let mut state = ListState::default()
        .with_selected(Some(selected_index.min(item_count.saturating_sub(1))));

    let title = format!("Add Workspace: {}", browser.cwd.display());

    let (top_area, list_area) = if show_top {
        let [filter_area, list_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(3)])
            .areas(popup);
        (Some(filter_area), list_area)
    } else {
        (None, popup)
    };

    let mut bottom_spans = vec![Span::raw(" ")];
    if browser.editing_path {
        bottom_spans.extend(footer_hint(theme, "Tab", "complete"));
        bottom_spans.extend(footer_hint(theme, "Enter", "add"));
        bottom_spans.extend(footer_hint(theme, "Ctrl-g", "browse"));
    } else if browser.searching {
        bottom_spans.extend(footer_hint(theme, "Enter", "done"));
        bottom_spans.extend(footer_hint(theme, "Esc", "clear"));
    } else if show_top {
        bottom_spans.extend(footer_hint(theme, "/", "search"));
        bottom_spans.extend(footer_hint(theme, "Enter/l", "open"));
        bottom_spans.extend(footer_hint(theme, "g", "go to"));
        bottom_spans.extend(footer_hint(theme, "Esc", "cancel"));
    } else {
        bottom_spans.extend(footer_hint(theme, "/", "search"));
        bottom_spans.extend(footer_hint(theme, "Enter/l", "open"));
        bottom_spans.extend(footer_hint(theme, "o", "add current"));
        bottom_spans.extend(footer_hint(theme, "g", "go to"));
        bottom_spans.extend(footer_hint(theme, "Esc", "cancel"));
    }

    if let Some(filter_area) = top_area {
        let (prefix, text, cursor) = if browser.editing_path {
            ("go: ", browser.path_input.text.as_str(), browser.path_input.cursor)
        } else {
            ("/ ", browser.filter.text.as_str(), browser.filter.cursor)
        };
        let input_block = themed_block(theme, &title);
        Paragraph::new(render_cursor_input(prefix, text, cursor, theme))
            .block(input_block)
            .render(filter_area, f.buffer_mut());

        let list_block = Block::default()
            .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.overlay_border))
            .style(Style::default().bg(theme.overlay_bg))
            .title_bottom(Line::from(bottom_spans));
        StatefulWidget::render(
            List::new(items)
                .block(list_block)
                .highlight_style(theme.selection_style()),
            list_area,
            f.buffer_mut(),
            &mut state,
        );
    } else {
        let list_block = themed_block(theme, &title).title_bottom(Line::from(bottom_spans));
        StatefulWidget::render(
            List::new(items)
                .block(list_block)
                .highlight_style(theme.selection_style()),
            list_area,
            f.buffer_mut(),
            &mut state,
        );
    }

    if let Some(err) = &browser.error {
        // Paint error into the status-adjacent area inside popup bottom.
        let err_area = Rect {
            x: popup.x + 1,
            y: popup.y + popup.height.saturating_sub(3),
            width: popup.width.saturating_sub(2),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("Error: {err}"),
                Style::default().fg(ratatui::style::Color::Red),
            )),
            err_area,
        );
    }
}
