//! Minimal single-line text input for overlay search / path / rename editor.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Default)]
pub struct LineInput {
    pub text: String,
    /// Byte offset into `text` (char boundary).
    pub cursor: usize,
}

/// Visible slice for a fixed-width field (horizontal scroll follows cursor).
#[derive(Debug, Clone)]
pub struct InputView {
    /// Display columns scrolled off the left edge.
    pub scroll_cols: usize,
    /// Glyphs to paint (clipped to the field width).
    pub visible: String,
    /// Cursor column within the field `[0, cols]` (may be past last glyph).
    pub cursor_col: usize,
}

impl LineInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_left(&mut self) {
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    pub fn insert_char(&mut self, ch: char) {
        let i = self.cursor.min(self.text.len());
        self.text.insert(i, ch);
        self.cursor = i + ch.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.insert_char(ch);
        }
    }

    pub fn backspace(&mut self) {
        let i = self.cursor.min(self.text.len());
        if i == 0 {
            return;
        }
        let prev = prev_boundary(&self.text, i);
        self.text.replace_range(prev..i, "");
        self.cursor = prev;
    }

    pub fn delete(&mut self) {
        let i = self.cursor.min(self.text.len());
        if i >= self.text.len() {
            return;
        }
        let next = next_boundary(&self.text, i);
        self.text.replace_range(i..next, "");
    }

    /// Kill from cursor to end of line (readline Ctrl+K).
    pub fn kill_to_end(&mut self) {
        let i = self.cursor.min(self.text.len());
        self.text.truncate(i);
    }

    /// Clear the whole line (amux Ctrl+U — 清空整行).
    pub fn clear_line(&mut self) {
        self.clear();
    }

    /// Fixed-width viewport: scroll so the cursor stays visible.
    pub fn view(&self, cols: usize) -> InputView {
        let cols = cols.max(1);
        let cursor = self.cursor.min(self.text.len());
        let cursor_col = UnicodeWidthStr::width(&self.text[..cursor]);

        // Keep cursor in [scroll, scroll + cols) — reserve rightmost cell for
        // an end-of-line caret when the cursor sits after the last glyph.
        let mut scroll = 0usize;
        if cursor_col >= cols {
            scroll = cursor_col + 1 - cols;
        }

        let mut visible = String::new();
        let mut col = 0usize;
        for ch in self.text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
            let next = col + cw;
            if next <= scroll {
                col = next;
                continue;
            }
            if col < scroll {
                // Wide char straddles the left edge — skip it.
                col = next;
                continue;
            }
            if col - scroll + cw > cols {
                break;
            }
            visible.push(ch);
            col = next;
        }

        InputView {
            scroll_cols: scroll,
            visible,
            cursor_col: cursor_col.saturating_sub(scroll).min(cols),
        }
    }

    /// Apply a complete raw sequence as a text-edit gesture.
    /// Returns true if the sequence was consumed as editing.
    pub fn handle_seq(&mut self, seq: &[u8]) -> bool {
        if let Some(action) = decode_edit_seq(seq) {
            match action {
                EditAction::Backspace => self.backspace(),
                EditAction::Delete => self.delete(),
                EditAction::Left => self.move_left(),
                EditAction::Right => self.move_right(),
                EditAction::Home => self.move_home(),
                EditAction::End => self.move_end(),
                EditAction::ClearLine => self.clear_line(),
                EditAction::KillToEnd => self.kill_to_end(),
                EditAction::BackspaceWord => self.backspace_word(),
                EditAction::Insert(ch) => self.insert_char(ch),
            }
            return true;
        }
        false
    }

    fn backspace_word(&mut self) {
        let i = self.cursor.min(self.text.len());
        if i == 0 {
            return;
        }
        let bytes = self.text.as_bytes();
        let mut j = i;
        while j > 0 && bytes[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        while j > 0 && !bytes[j - 1].is_ascii_whitespace() {
            j = prev_boundary(&self.text, j);
        }
        self.text.replace_range(j..i, "");
        self.cursor = j;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditAction {
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    ClearLine,
    KillToEnd,
    BackspaceWord,
    Insert(char),
}

fn decode_edit_seq(seq: &[u8]) -> Option<EditAction> {
    match seq {
        // Backspace / Ctrl+H
        b"\x7f" | b"\x08" => return Some(EditAction::Backspace),
        b"\x1b[3~" => return Some(EditAction::Delete),
        b"\x1b[D" => return Some(EditAction::Left),
        b"\x1b[C" => return Some(EditAction::Right),
        b"\x1b[H" | b"\x1b[1~" | b"\x1b[7~" => return Some(EditAction::Home),
        b"\x1b[F" | b"\x1b[4~" | b"\x1b[8~" => return Some(EditAction::End),
        // Emacs / readline C0 controls
        b"\x01" => return Some(EditAction::Home),          // Ctrl+A
        b"\x05" => return Some(EditAction::End),           // Ctrl+E
        b"\x02" => return Some(EditAction::Left),          // Ctrl+B
        b"\x06" => return Some(EditAction::Right),         // Ctrl+F
        b"\x04" => return Some(EditAction::Delete),        // Ctrl+D
        b"\x0b" => return Some(EditAction::KillToEnd),     // Ctrl+K
        b"\x15" => return Some(EditAction::ClearLine),     // Ctrl+U
        b"\x17" => return Some(EditAction::BackspaceWord), // Ctrl+W
        _ => {}
    }

    if let Some(action) = decode_ctrl_letter_seq(seq) {
        return Some(action);
    }

    if let Ok(s) = std::str::from_utf8(seq) {
        if s.chars().count() == 1 {
            let ch = s.chars().next().unwrap();
            if !ch.is_control() {
                return Some(EditAction::Insert(ch));
            }
        }
    }
    None
}

/// Map Kitty `CSI codepoint ; mods u` and xterm `CSI 27 ; mods ; code ~`
/// Ctrl bindings to edit actions.
fn decode_ctrl_letter_seq(seq: &[u8]) -> Option<EditAction> {
    let (codepoint, mods) = parse_encoded_key_seq(seq)?;
    // Kitty/xterm: modifiers = 1 + (shift=1|alt=2|ctrl=4|…)
    let ctrl = (mods.saturating_sub(1) & 4) != 0;
    if !ctrl {
        return None;
    }
    let ch = char::from_u32(codepoint)?;
    match ch.to_ascii_lowercase() {
        'a' => Some(EditAction::Home),
        'e' => Some(EditAction::End),
        'b' => Some(EditAction::Left),
        'f' => Some(EditAction::Right),
        'd' => Some(EditAction::Delete),
        'h' => Some(EditAction::Backspace),
        'k' => Some(EditAction::KillToEnd),
        'u' => Some(EditAction::ClearLine),
        'w' => Some(EditAction::BackspaceWord),
        _ => None,
    }
}

/// Returns (unicode codepoint, modifier field) for Kitty CSI-u or modifyOtherKeys.
fn parse_encoded_key_seq(seq: &[u8]) -> Option<(u32, u32)> {
    if seq.len() < 4 || seq[0] != 0x1b || seq[1] != b'[' {
        return None;
    }
    let last = *seq.last()?;

    // Kitty: CSI <codepoint> ; <mods> u   or  CSI <codepoint> ; <mods>:<event> u
    if last == b'u' {
        let inner = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
        let mut parts = inner.split(';');
        let cp: u32 = parts.next()?.parse().ok()?;
        let mods_field = parts.next().unwrap_or("1");
        // Drop release events (:3)
        let (mods_str, event) = match mods_field.split_once(':') {
            Some((m, e)) => (m, Some(e)),
            None => (mods_field, None),
        };
        if event.is_some_and(|e| e.starts_with('3')) {
            return None;
        }
        let mods: u32 = mods_str.parse().ok()?;
        return Some((cp, mods));
    }

    // modifyOtherKeys: CSI 27 ; <mods> ; <code> ~
    if last == b'~' {
        let inner = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
        let mut parts = inner.split(';');
        if parts.next()? != "27" {
            return None;
        }
        let mods: u32 = parts.next()?.parse().ok()?;
        let cp: u32 = parts.next()?.parse().ok()?;
        return Some((cp, mods));
    }

    None
}

fn prev_boundary(s: &str, i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    let mut j = i - 1;
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

fn next_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_u_clears_legacy_and_kitty() {
        let mut inp = LineInput::new();
        inp.set_text("hello world".into());
        assert!(inp.handle_seq(b"\x15"));
        assert!(inp.is_empty());

        inp.set_text("again".into());
        assert!(inp.handle_seq(b"\x1b[117;5u"));
        assert!(inp.is_empty());
    }

    #[test]
    fn ctrl_k_kills_to_end() {
        let mut inp = LineInput::new();
        inp.set_text("hello world".into());
        inp.cursor = 5;
        assert!(inp.handle_seq(b"\x0b"));
        assert_eq!(inp.text, "hello");
    }

    #[test]
    fn ctrl_a_e_home_end() {
        let mut inp = LineInput::new();
        inp.set_text("abc".into());
        assert!(inp.handle_seq(b"\x01"));
        assert_eq!(inp.cursor, 0);
        assert!(inp.handle_seq(b"\x05"));
        assert_eq!(inp.cursor, 3);
    }

    #[test]
    fn view_scrolls_with_cursor() {
        let mut inp = LineInput::new();
        inp.set_text("abcdefghijklmnopqrstuvwxyz".into());
        inp.cursor = inp.text.len();
        let v = inp.view(10);
        assert!(UnicodeWidthStr::width(v.visible.as_str()) <= 10);
        assert!(v.visible.ends_with('z'), "{}", v.visible);
        assert_eq!(v.cursor_col, UnicodeWidthStr::width(v.visible.as_str()));
    }

    #[test]
    fn view_cjk_width() {
        let mut inp = LineInput::new();
        inp.set_text("中文测试标题很长".into());
        inp.move_end();
        let v = inp.view(8);
        assert!(UnicodeWidthStr::width(v.visible.as_str()) <= 8);
        assert!(v.cursor_col <= 8);
    }

    #[test]
    fn modify_other_keys_ctrl_u() {
        let mut inp = LineInput::new();
        inp.set_text("x".into());
        assert!(inp.handle_seq(b"\x1b[27;5;117~"));
        assert!(inp.is_empty());
    }
}
