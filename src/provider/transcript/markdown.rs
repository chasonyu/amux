//! Lightweight markdown → plain lines with style hints (v1 subset).

/// One rendered markdown line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdLine {
    pub text: String,
    pub kind: MdKind,
}

/// Style hint for a rendered markdown line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdKind {
    Normal,
    Heading,
    Code,
    List,
    Dim,
}

/// Render a markdown subset to wrapped lines.
///
/// Supports ATX headings, unordered lists (`- `/`* `), fenced code,
/// simple `**bold**` / `*italic*` (markers stripped), paragraphs, and
/// hard wrap to `width`.
pub fn render_markdown(src: &str, width: usize) -> Vec<MdLine> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut para = String::new();

    let flush_para = |para: &mut String, out: &mut Vec<MdLine>| {
        if para.is_empty() {
            return;
        }
        let text = apply_inline(para.trim());
        para.clear();
        if text.is_empty() {
            return;
        }
        for line in wrap_text(&text, width) {
            out.push(MdLine {
                text: line,
                kind: MdKind::Normal,
            });
        }
    };

    for raw in src.lines() {
        let line = raw.trim_end();

        if line.starts_with("```") {
            flush_para(&mut para, &mut out);
            in_fence = !in_fence;
            out.push(MdLine {
                text: line.to_string(),
                kind: MdKind::Code,
            });
            continue;
        }

        if in_fence {
            out.push(MdLine {
                text: line.to_string(),
                kind: MdKind::Code,
            });
            continue;
        }

        if line.trim().is_empty() {
            flush_para(&mut para, &mut out);
            continue;
        }

        if let Some(heading) = parse_atx_heading(line) {
            flush_para(&mut para, &mut out);
            let text = apply_inline(&heading);
            for wrapped in wrap_text(&text, width) {
                out.push(MdLine {
                    text: wrapped,
                    kind: MdKind::Heading,
                });
            }
            continue;
        }

        if let Some(item) = parse_unordered_list(line) {
            flush_para(&mut para, &mut out);
            let text = format!("- {}", apply_inline(&item));
            for wrapped in wrap_text(&text, width) {
                out.push(MdLine {
                    text: wrapped,
                    kind: MdKind::List,
                });
            }
            continue;
        }

        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(line.trim());
    }

    flush_para(&mut para, &mut out);
    out
}

fn parse_atx_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some(rest.trim().to_string())
}

fn parse_unordered_list(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return Some(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("* ") {
        // Avoid treating `*italic*` alone as a list when no space after first *.
        return Some(rest.to_string());
    }
    None
}

/// Strip simple `**bold**` and `*italic*` markers; leave other text as-is.
fn apply_inline(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_close(&chars, i + 2, "**") {
                for c in &chars[i + 2..end] {
                    out.push(*c);
                }
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '*' {
            if let Some(end) = find_close(&chars, i + 1, "*") {
                for c in &chars[i + 1..end] {
                    out.push(*c);
                }
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_close(chars: &[char], start: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let mut i = start;
    while i + m.len() <= chars.len() {
        if chars[i..i + m.len()] == m[..] && i > start {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            if word.chars().count() <= width {
                current.push_str(word);
            } else {
                // Hard-break overlong tokens.
                let mut chunk = String::new();
                for ch in word.chars() {
                    if chunk.chars().count() >= width {
                        lines.push(std::mem::take(&mut chunk));
                    }
                    chunk.push(ch);
                }
                current = chunk;
            }
            continue;
        }
        let next_len = current.chars().count() + 1 + word.chars().count();
        if next_len <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            if word.chars().count() <= width {
                current.push_str(word);
            } else {
                let mut chunk = String::new();
                for ch in word.chars() {
                    if chunk.chars().count() >= width {
                        lines.push(std::mem::take(&mut chunk));
                    }
                    chunk.push(ch);
                }
                current = chunk;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_and_heading() {
        let lines = render_markdown("# Hi\n\n```rs\nlet x=1;\n```\n", 40);
        assert!(lines.iter().any(|l| l.text.contains("Hi")));
        assert!(lines.iter().any(|l| l.text.contains("```")));
    }

    #[test]
    fn list_and_inline() {
        let lines = render_markdown("- **bold** and *italic*\n", 40);
        assert!(lines.iter().any(|l| l.kind == MdKind::List));
        let joined: String = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(joined.contains("bold"));
        assert!(joined.contains("italic"));
        assert!(!joined.contains("**"));
        assert!(!joined.contains('*'));
    }
}
