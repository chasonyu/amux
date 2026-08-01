//! Minimal single-line text input for overlay search / path editor.

#[derive(Debug, Clone, Default)]
pub struct LineInput {
    pub text: String,
    pub cursor: usize,
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

    /// Apply a complete raw sequence as a text-edit gesture.
    /// Returns true if the sequence was consumed as editing.
    pub fn handle_seq(&mut self, seq: &[u8]) -> bool {
        match seq {
            b"\x7f" | b"\x08" => {
                self.backspace();
                true
            }
            b"\x1b[3~" => {
                self.delete();
                true
            }
            b"\x1b[D" => {
                self.move_left();
                true
            }
            b"\x1b[C" => {
                self.move_right();
                true
            }
            b"\x1b[H" | b"\x1b[1~" => {
                self.move_home();
                true
            }
            b"\x1b[F" | b"\x1b[4~" => {
                self.move_end();
                true
            }
            b"\x15" => {
                // Ctrl+U — clear line
                self.clear();
                true
            }
            b"\x17" => {
                // Ctrl+W — backspace word
                self.backspace_word();
                true
            }
            _ => {
                if let Ok(s) = std::str::from_utf8(seq) {
                    if s.chars().count() == 1 {
                        let ch = s.chars().next().unwrap();
                        if !ch.is_control() {
                            self.insert_char(ch);
                            return true;
                        }
                    }
                }
                false
            }
        }
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
