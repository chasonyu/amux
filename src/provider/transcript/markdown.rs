//! Lightweight markdown → styled span lines (omp-aligned inline styling).
//!
//! Renders a subset aligned with omp's default collapsed look: ATX headings,
//! ordered/unordered lists, fenced code (lang preserved), blockquotes, hr,
//! and inline `**bold**` / `*italic*` / `` `code` `` / `[text](url)` styled as
//! spans rather than stripped to plain text.

use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Inline style kind for a markdown span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdKind {
    Normal,
    Bold,
    Italic,
    Code,
    Heading,
    Link,
    Dim,
    ListBullet,
    CodeBlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdSpan {
    pub text: String,
    pub kind: MdKind,
}

/// One rendered markdown line; spans carry inline styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdLine {
    pub spans: Vec<MdSpan>,
}

impl MdLine {
    fn plain(text: impl Into<String>, kind: MdKind) -> Self {
        Self {
            spans: vec![MdSpan {
                text: text.into(),
                kind,
            }],
        }
    }

    fn empty() -> Self {
        Self {
            spans: vec![MdSpan {
                text: String::new(),
                kind: MdKind::Normal,
            }],
        }
    }
}

/// Render a markdown subset to wrapped, style-bearing lines.
pub fn render_markdown(src: &str, width: usize) -> Vec<MdLine> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut lines = src.lines().peekable();

    while let Some(raw) = lines.next() {
        let trimmed = raw.trim_end();

        // Fenced code block
        if let Some(lang) = parse_fence(trimmed) {
            out.push(MdLine::plain(format!("```{lang}"), MdKind::CodeBlock));
            let inner_w = width.saturating_sub(2).max(1);
            while let Some(cl) = lines.next() {
                let ct = cl.trim_end();
                if parse_fence(ct).is_some() {
                    break;
                }
                for w in wrap_text(ct, inner_w) {
                    out.push(MdLine::plain(format!("  {w}"), MdKind::CodeBlock));
                }
            }
            out.push(MdLine::plain("```", MdKind::CodeBlock));
            continue;
        }

        // Horizontal rule
        if is_hr(trimmed) {
            let n = width.min(80);
            out.push(MdLine::plain("─".repeat(n), MdKind::Dim));
            continue;
        }

        // ATX heading
        if let Some((level, text)) = parse_heading(trimmed) {
            let prefix = if level <= 2 {
                String::new()
            } else {
                format!("{} ", "#".repeat(level))
            };
            let head = format!("{prefix}{text}");
            for w in wrap_text(&head, width) {
                out.push(MdLine {
                    spans: parse_inline(&w, MdKind::Heading),
                });
            }
            continue;
        }

        // Blockquote
        if let Some(inner) = parse_blockquote(trimmed) {
            let inner_w = width.saturating_sub(2).max(1);
            for w in wrap_text(&inner, inner_w) {
                out.push(MdLine {
                    spans: vec![
                        MdSpan {
                            text: "│ ".into(),
                            kind: MdKind::Dim,
                        },
                        MdSpan {
                            text: w,
                            kind: MdKind::Italic,
                        },
                    ],
                });
            }
            continue;
        }

        // Ordered list
        if let Some((num, text)) = parse_ordered_list(trimmed) {
            let marker = format!("{num}. ");
            let mw = marker.width();
            let inner_w = width.saturating_sub(mw).max(1);
            for (i, w) in wrap_text(&text, inner_w).into_iter().enumerate() {
                if i == 0 {
                    out.push(MdLine {
                        spans: vec![
                            MdSpan {
                                text: marker.clone(),
                                kind: MdKind::ListBullet,
                            },
                            MdSpan {
                                text: w,
                                kind: MdKind::Normal,
                            },
                        ],
                    });
                } else {
                    out.push(MdLine {
                        spans: vec![
                            MdSpan {
                                text: " ".repeat(mw),
                                kind: MdKind::Normal,
                            },
                            MdSpan {
                                text: w,
                                kind: MdKind::Normal,
                            },
                        ],
                    });
                }
            }
            continue;
        }

        // Unordered list
        if let Some(text) = parse_unordered_list(trimmed) {
            let marker = "• ";
            let mw = marker.width();
            let inner_w = width.saturating_sub(mw).max(1);
            for (i, w) in wrap_text(&text, inner_w).into_iter().enumerate() {
                if i == 0 {
                    out.push(MdLine {
                        spans: vec![
                            MdSpan {
                                text: marker.into(),
                                kind: MdKind::ListBullet,
                            },
                            MdSpan {
                                text: w,
                                kind: MdKind::Normal,
                            },
                        ],
                    });
                } else {
                    out.push(MdLine {
                        spans: vec![
                            MdSpan {
                                text: " ".repeat(mw),
                                kind: MdKind::Normal,
                            },
                            MdSpan {
                                text: w,
                                kind: MdKind::Normal,
                            },
                        ],
                    });
                }
            }
            continue;
        }

        // Blank line
        if trimmed.is_empty() {
            out.push(MdLine::empty());
            continue;
        }

        // Paragraph
        for w in wrap_text(trimmed, width) {
            out.push(MdLine {
                spans: parse_inline(&w, MdKind::Normal),
            });
        }
    }

    out
}

/// Parse inline markers (`**bold**`, `*italic*`, `` `code` ``, `[text](url)`)
/// into styled spans; non-marker text uses `base`.
fn parse_inline(text: &str, base: MdKind) -> Vec<MdSpan> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((content, kind, consumed)) = match_inline(rest) {
            if !buf.is_empty() {
                spans.push(MdSpan {
                    text: std::mem::take(&mut buf),
                    kind: base,
                });
            }
            spans.push(MdSpan {
                text: content,
                kind,
            });
            rest = &rest[consumed..];
        } else {
            let ch = rest.chars().next().unwrap();
            buf.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    if !buf.is_empty() {
        spans.push(MdSpan {
            text: buf,
            kind: base,
        });
    }
    if spans.is_empty() {
        spans.push(MdSpan {
            text: String::new(),
            kind: base,
        });
    }
    spans
}

/// Try to match an inline marker at the start of `rest`.
/// Returns (content, kind, consumed bytes) or None.
fn match_inline(rest: &str) -> Option<(String, MdKind, usize)> {
    // **bold**
    if rest.starts_with("**") {
        if let Some(end) = rest[2..].find("**") {
            return Some((rest[2..2 + end].to_string(), MdKind::Bold, 2 + end + 2));
        }
    }
    // `code`
    if rest.starts_with('`') {
        if let Some(end) = rest[1..].find('`') {
            return Some((rest[1..1 + end].to_string(), MdKind::Code, 1 + end + 1));
        }
    }
    // *italic* (single star, not **)
    if rest.starts_with('*') && !rest.starts_with("**") {
        if let Some(end) = rest[1..].find('*') {
            return Some((rest[1..1 + end].to_string(), MdKind::Italic, 1 + end + 1));
        }
    }
    // [text](url)
    if rest.starts_with('[') {
        if let Some(close) = rest.find("](") {
            if let Some(pclose) = rest[close + 2..].find(')') {
                return Some((
                    rest[1..close].to_string(),
                    MdKind::Link,
                    close + 2 + pclose + 1,
                ));
            }
        }
    }
    None
}

/// Detect a fenced code opener: `` ``` `` or `~~~`, returning the info string.
fn parse_fence(line: &str) -> Option<String> {
    let t = line.trim_start();
    let fence_len = t
        .chars()
        .take_while(|c| *c == '`' || *c == '~')
        .count();
    if fence_len >= 3 && t[..fence_len].chars().all(|c| c == '`' || c == '~') {
        let lang = t[fence_len..].trim();
        Some(lang.to_string())
    } else {
        None
    }
}

/// A horizontal rule: 3+ of the same marker (`-`, `*`, `_`, `─`, `=`).
fn is_hr(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let chars: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 3 {
        return false;
    }
    let c = chars[0];
    matches!(c, '-' | '*' | '_' | '─' | '=') && chars.iter().all(|&x| x == c)
}

/// ATX heading: 1–6 `#` then text. Returns (level, text).
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = t[level..].trim();
    Some((level, rest.to_string()))
}

/// Blockquote line: `> text` or `>`.
fn parse_blockquote(line: &str) -> Option<String> {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("> ") {
        Some(rest.to_string())
    } else if t == ">" {
        Some(String::new())
    } else {
        None
    }
}

/// Ordered list: `N.` or `N)` then text.
fn parse_ordered_list(line: &str) -> Option<(String, String)> {
    let t = line.trim_start();
    let mut digits = String::new();
    for (i, c) in t.chars().enumerate() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if (c == '.' || c == ')') && !digits.is_empty() {
            let rest = t[i + 1..].trim_start();
            return Some((digits, rest.to_string()));
        } else {
            return None;
        }
    }
    None
}

/// Unordered list: `- ` or `* ` then text.
fn parse_unordered_list(line: &str) -> Option<String> {
    let t = line.trim_start();
    t.strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .map(|s| s.to_string())
}

/// Hard-wrap `text` to `width` visible columns, breaking on spaces; words
/// longer than `width` are wrapped by character. Never returns empty.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut line_w = 0usize;

    for word in text.split_whitespace() {
        let ww = UnicodeWidthStr::width(word);
        let need = if line.is_empty() { ww } else { line_w + 1 + ww };
        if need <= width {
            if !line.is_empty() {
                line.push(' ');
                line_w += 1;
            }
            line.push_str(word);
            line_w += ww;
        } else {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if ww <= width {
                line.push_str(word);
                line_w = ww;
            } else {
                let mut cur = String::new();
                let mut cur_w = 0;
                for ch in word.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
                    if cur_w + cw > width && !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                    cur.push(ch);
                    cur_w += cw;
                }
                line = cur;
                line_w = cur_w;
            }
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(line: &MdLine) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn bold_italic_code_styled_not_stripped() {
        let lines = render_markdown("hello **world** and `code`", 80);
        let spans = &lines[0].spans;
        let bold = spans.iter().find(|s| s.kind == MdKind::Bold);
        assert_eq!(bold.map(|s| s.text.as_str()), Some("world"));
        let code = spans.iter().find(|s| s.kind == MdKind::Code);
        assert_eq!(code.map(|s| s.text.as_str()), Some("code"));
        assert!(spans.iter().any(|s| s.kind == MdKind::Normal && s.text.contains("hello")));
    }

    #[test]
    fn italic_single_star() {
        let lines = render_markdown("a *b* c", 80);
        let spans = &lines[0].spans;
        let italic = spans.iter().find(|s| s.kind == MdKind::Italic);
        assert_eq!(italic.map(|s| s.text.as_str()), Some("b"));
    }

    #[test]
    fn link_text_only() {
        let lines = render_markdown("see [docs](https://x) here", 80);
        let spans = &lines[0].spans;
        let link = spans.iter().find(|s| s.kind == MdKind::Link);
        assert_eq!(link.map(|s| s.text.as_str()), Some("docs"));
    }

    #[test]
    fn fenced_code_preserves_lang_and_indents() {
        let src = "```rust\nlet x = 1;\n```\n";
        let lines = render_markdown(src, 80);
        assert!(plain_text(&lines[0]).contains("```rust"));
        assert!(plain_text(&lines[1]).contains("  let x = 1;"));
        assert_eq!(plain_text(&lines[2]), "```");
    }

    #[test]
    fn heading_kept_and_styled() {
        let lines = render_markdown("# Title", 80);
        assert_eq!(lines[0].spans[0].kind, MdKind::Heading);
        assert!(plain_text(&lines[0]).contains("Title"));
    }

    #[test]
    fn h3_keeps_prefix() {
        let lines = render_markdown("### Sub", 80);
        assert!(plain_text(&lines[0]).contains("### Sub"));
    }

    #[test]
    fn ordered_list_numbered() {
        let lines = render_markdown("1. first\n2. second", 80);
        assert!(plain_text(&lines[0]).starts_with("1. first"));
        assert!(plain_text(&lines[1]).starts_with("2. second"));
        assert_eq!(lines[0].spans[0].kind, MdKind::ListBullet);
    }

    #[test]
    fn unordered_list_bullet() {
        let lines = render_markdown("- item", 80);
        assert!(plain_text(&lines[0]).starts_with("• item"));
        assert_eq!(lines[0].spans[0].kind, MdKind::ListBullet);
    }

    #[test]
    fn blockquote_border_and_italic() {
        let lines = render_markdown("> quoted", 80);
        assert_eq!(lines[0].spans[0].text, "│ ");
        assert_eq!(lines[0].spans[0].kind, MdKind::Dim);
        assert_eq!(lines[0].spans[1].text, "quoted");
        assert_eq!(lines[0].spans[1].kind, MdKind::Italic);
    }

    #[test]
    fn hr_renders_rule() {
        let lines = render_markdown("---", 80);
        assert!(plain_text(&lines[0]).chars().all(|c| c == '─'));
        assert_eq!(lines[0].spans[0].kind, MdKind::Dim);
    }

    #[test]
    fn long_word_wraps_by_char() {
        let lines = render_markdown("aaaaaaaaaaaaaaaaaaaa", 5);
        assert!(lines.iter().all(|l| plain_text(l).chars().count() <= 5));
    }
}
