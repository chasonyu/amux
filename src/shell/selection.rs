//! Pane-clipped stream selection (tmux-like) for the Agent content area.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::pty::{SnapshotCell, TerminalSnapshot};

/// Active drag / completed highlight inside one content rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSelection {
    /// Screen-space content area selection is relative to.
    pub area: Rect,
    /// Pane-local 0-based anchor (press).
    pub anchor: (u16, u16),
    /// Pane-local 0-based end (current pointer).
    pub end: (u16, u16),
    /// True once the pointer left the anchor cell.
    pub dragged: bool,
}

impl PaneSelection {
    pub fn begin(area: Rect, local_row: u16, local_col: u16) -> Self {
        let p = clamp_local(area, local_row, local_col);
        Self {
            area,
            anchor: p,
            end: p,
            dragged: false,
        }
    }

    pub fn update_end(&mut self, screen_x: u16, screen_y: u16) {
        let local = screen_to_local(self.area, screen_x, screen_y);
        if local != self.anchor {
            self.dragged = true;
        }
        self.end = local;
    }

    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        normalize_stream(self.anchor, self.end)
    }
}

/// Clamp pane-local coords into `area` size.
pub fn clamp_local(area: Rect, row: u16, col: u16) -> (u16, u16) {
    let max_r = area.height.saturating_sub(1);
    let max_c = area.width.saturating_sub(1);
    (row.min(max_r), col.min(max_c))
}

/// Screen (0-based) → pane-local, clamped.
pub fn screen_to_local(area: Rect, x: u16, y: u16) -> (u16, u16) {
    let row = y.saturating_sub(area.y);
    let col = x.saturating_sub(area.x);
    clamp_local(area, row, col)
}

/// Reading-order stream endpoints `(start, end)` inclusive.
pub fn normalize_stream(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.0, a.1) <= (b.0, b.1) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Whether `(row,col)` lies in an inclusive stream selection.
pub fn cell_in_stream(row: u16, col: u16, a: (u16, u16), b: (u16, u16)) -> bool {
    let (s, e) = normalize_stream(a, b);
    if s.0 == e.0 {
        row == s.0 && col >= s.1 && col <= e.1
    } else if row == s.0 {
        col >= s.1
    } else if row == e.0 {
        col <= e.1
    } else {
        row > s.0 && row < e.0
    }
}

/// Build OSC 52 clipboard-set sequence (clipboard `c`).
pub fn osc52_clipboard_set(text: &str) -> Vec<u8> {
    let b64 = base64_encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07").into_bytes()
}

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

/// Extract stream-selected text from a live PTY snapshot (viewport only).
pub fn text_from_snapshot(snap: &TerminalSnapshot, a: (u16, u16), b: (u16, u16)) -> String {
    use unicode_width::UnicodeWidthStr;
    let (s, e) = normalize_stream(a, b);
    // Default `" "` = empty cell; `""` = wide-char spacer (must not be copied).
    let mut grid: Vec<Vec<String>> = (0..snap.rows)
        .map(|_| (0..snap.cols).map(|_| " ".to_string()).collect())
        .collect();
    for cell in &snap.cells {
        let r = cell.row as usize;
        let c = cell.col as usize;
        if r >= grid.len() || c >= snap.cols as usize {
            continue;
        }
        grid[r][c] = cell.symbol.clone();
        // alacritty skips WIDE_CHAR_SPACER cells — mark them so extract
        // does not emit a literal space between CJK glyphs.
        let w = UnicodeWidthStr::width(cell.symbol.as_str()).max(1);
        for i in 1..w {
            if c + i < snap.cols as usize {
                grid[r][c + i] = String::new();
            }
        }
    }
    extract_from_grid(&grid, s, e)
}

/// Extract from a rectangular grid of cell strings (row-major).
/// Empty string cells are wide-char spacers and are skipped.
pub fn extract_from_grid(grid: &[Vec<String>], s: (u16, u16), e: (u16, u16)) -> String {
    let (s, e) = normalize_stream(s, e);
    let mut out = String::new();
    for row in s.0..=e.0 {
        let Some(line) = grid.get(row as usize) else {
            continue;
        };
        let (c0, c1) = if s.0 == e.0 {
            (s.1 as usize, e.1 as usize)
        } else if row == s.0 {
            (s.1 as usize, line.len().saturating_sub(1))
        } else if row == e.0 {
            (0, e.1 as usize)
        } else {
            (0, line.len().saturating_sub(1))
        };
        if line.is_empty() {
            if row != e.0 {
                out.push('\n');
            }
            continue;
        }
        let c1 = c1.min(line.len().saturating_sub(1));
        if c0 <= c1 {
            for col in c0..=c1 {
                if !cell_in_stream(row, col as u16, s, e) {
                    continue;
                }
                let sym = &line[col];
                if sym.is_empty() {
                    continue; // wide-char spacer
                }
                out.push_str(sym);
            }
        }
        // Trim trailing spaces on each line for nicer paste.
        while out.ends_with(' ') {
            out.pop();
        }
        if row != e.0 {
            out.push('\n');
        }
    }
    out
}

/// Flatten rendered transcript lines (already windowed) into a char-cell grid
/// of `width` columns using unicode display width.
pub fn grid_from_plain_lines(lines: &[String], width: usize) -> Vec<Vec<String>> {
    use unicode_width::UnicodeWidthChar;
    let mut grid = Vec::with_capacity(lines.len());
    for line in lines {
        let mut row = vec![" ".to_string(); width];
        let mut col = 0usize;
        for ch in line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if col >= width {
                break;
            }
            row[col] = ch.to_string();
            // Wide char: following columns are spacers (not copyable spaces).
            for i in 1..w {
                if col + i < width {
                    row[col + i] = String::new();
                }
            }
            col += w;
        }
        grid.push(row);
    }
    grid
}

/// Paint selection with solid theme colors (no REVERSED — that washed out the bg).
pub fn paint_selection_overlay(buf: &mut Buffer, sel: &PaneSelection, style: Style) {
    if sel.area.width == 0 || sel.area.height == 0 {
        return;
    }
    let (s, e) = sel.normalized();
    for row in 0..sel.area.height {
        for col in 0..sel.area.width {
            if !cell_in_stream(row, col, s, e) {
                continue;
            }
            let x = sel.area.x + col;
            let y = sel.area.y + row;
            if x >= buf.area.x.saturating_add(buf.area.width)
                || y >= buf.area.y.saturating_add(buf.area.height)
            {
                continue;
            }
            let cell = &mut buf[(x, y)];
            // Replace style entirely so underlying cell colors cannot mute the fill.
            cell.set_style(style);
        }
    }
}

/// Left-button event? (low 2 bits of Cb == 0), ignoring motion/mods/wheel.
pub fn sgr_is_left_button(seq: &[u8]) -> bool {
    sgr_cb(seq).is_some_and(|cb| cb & 64 == 0 && cb & 3 == 0)
}

/// Alt/Meta modifier (xterm bit 3 = 8).
pub fn sgr_has_meta(seq: &[u8]) -> bool {
    sgr_cb(seq).is_some_and(|cb| cb & 8 != 0)
}

/// Motion/drag bit (32).
pub fn sgr_is_motion(seq: &[u8]) -> bool {
    sgr_cb(seq).is_some_and(|cb| cb & 32 != 0)
}

fn sgr_cb(seq: &[u8]) -> Option<u16> {
    if seq.len() < 5 || seq[0] != 0x1b || seq[1] != b'[' || seq[2] != b'<' {
        return None;
    }
    let params = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    params.split(';').next()?.parse().ok()
}

/// Unit-test helper: sparse cells → snapshot-shaped extract.
#[cfg(test)]
pub fn snapshot_from_cells(rows: u16, cols: u16, cells: Vec<SnapshotCell>) -> TerminalSnapshot {
    TerminalSnapshot {
        rows,
        cols,
        cursor: None,
        cells,
        cursor_visible: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::SnapshotCell;
    use ratatui::style::{Color, Modifier};

    fn cell(row: u16, col: u16, ch: char) -> SnapshotCell {
        SnapshotCell {
            row,
            col,
            symbol: ch.to_string(),
            fg: Color::Reset,
            bg: Color::Reset,
            modifier: Modifier::empty(),
        }
    }

    #[test]
    fn normalize_orders_reading() {
        assert_eq!(
            normalize_stream((2, 5), (1, 3)),
            ((1, 3), (2, 5))
        );
    }

    #[test]
    fn stream_same_line() {
        assert!(cell_in_stream(0, 2, (0, 1), (0, 4)));
        assert!(!cell_in_stream(0, 0, (0, 1), (0, 4)));
        assert!(!cell_in_stream(1, 2, (0, 1), (0, 4)));
    }

    #[test]
    fn stream_multi_line() {
        let a = (0, 2);
        let b = (2, 1);
        assert!(cell_in_stream(0, 3, a, b));
        assert!(cell_in_stream(1, 0, a, b));
        assert!(cell_in_stream(2, 0, a, b));
        assert!(!cell_in_stream(2, 2, a, b));
        assert!(!cell_in_stream(0, 1, a, b));
    }

    #[test]
    fn extract_snapshot_block() {
        let mut cells = Vec::new();
        for (i, ch) in "hello".chars().enumerate() {
            cells.push(cell(0, i as u16, ch));
        }
        for (i, ch) in "world".chars().enumerate() {
            cells.push(cell(1, i as u16, ch));
        }
        let snap = snapshot_from_cells(2, 5, cells);
        let t = text_from_snapshot(&snap, (0, 1), (1, 3));
        assert_eq!(t, "ello\nworl");
    }

    #[test]
    fn extract_cjk_no_spacer_spaces() {
        // CJK glyphs are width-2; spacer columns must not become ' '.
        let cells = vec![
            cell(0, 0, '中'),
            cell(0, 2, '文'),
            cell(0, 4, '测'),
        ];
        let snap = snapshot_from_cells(1, 6, cells);
        let t = text_from_snapshot(&snap, (0, 0), (0, 5));
        assert_eq!(t, "中文测");
    }

    #[test]
    fn plain_lines_cjk_grid_extract() {
        let grid = grid_from_plain_lines(&["中文".into()], 4);
        let t = extract_from_grid(&grid, (0, 0), (0, 3));
        assert_eq!(t, "中文");
    }

    #[test]
    fn osc52_roundtrip_prefix() {
        let seq = osc52_clipboard_set("ab");
        let s = String::from_utf8(seq).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with('\x07'));
    }
}
