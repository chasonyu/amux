//! Lightweight markdown → styled span lines (omp-aligned inline styling).
//!
//! Renders a subset aligned with omp's default collapsed look: ATX headings,
//! ordered/unordered lists, fenced code (lang preserved), blockquotes, hr,
//! GFM tables (box borders + width-adaptive columns, ported from omp
//! `#renderTable`), and inline `**bold**` / `*italic*` / `` `code` `` /
//! `[text](url)` styled as spans rather than stripped to plain text.

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
                let mut spans = vec![MdSpan {
                    text: "│ ".into(),
                    kind: MdKind::Dim,
                }];
                // Keep quote italic as base; still strip ** / ` inside.
                spans.extend(parse_inline(&w, MdKind::Italic));
                out.push(MdLine { spans });
            }
            continue;
        }

        // Ordered list
        if let Some((num, text)) = parse_ordered_list(trimmed) {
            let marker = format!("{num}. ");
            let mw = marker.width();
            let inner_w = width.saturating_sub(mw).max(1);
            out.extend(render_prefix_lines(
                &marker,
                MdKind::ListBullet,
                &text,
                inner_w,
                mw,
            ));
            continue;
        }

        // Unordered list
        if let Some(text) = parse_unordered_list(trimmed) {
            let marker = "• ";
            let mw = marker.width();
            let inner_w = width.saturating_sub(mw).max(1);
            out.extend(render_prefix_lines(
                marker,
                MdKind::ListBullet,
                &text,
                inner_w,
                mw,
            ));
            continue;
        }

        // GFM table: header + |---| separator + body rows (omp `#renderTable`).
        if looks_like_table_row(trimmed) {
            if let Some(sep) = lines.peek().copied() {
                if is_table_separator(sep.trim_end()) {
                    lines.next(); // consume separator
                    let header = split_table_row(trimmed);
                    let mut rows = Vec::new();
                    while let Some(next) = lines.peek().copied() {
                        let nt = next.trim_end();
                        if nt.is_empty() || !looks_like_table_row(nt) || is_table_separator(nt) {
                            break;
                        }
                        rows.push(split_table_row(nt));
                        lines.next();
                    }
                    out.extend(render_table(&header, &rows, width));
                    continue;
                }
            }
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

// ── GFM table (omp-aligned) ─────────────────────────────────────────────

const TABLE_MAX_WORD: usize = 30;

fn looks_like_table_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('|') && !is_table_separator(t)
}

fn is_table_separator(line: &str) -> bool {
    let cells = split_table_row(line);
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| {
        let mut t = c.trim();
        if t.is_empty() {
            return false;
        }
        if let Some(rest) = t.strip_prefix(':') {
            t = rest;
        }
        if let Some(rest) = t.strip_suffix(':') {
            t = rest;
        }
        let dashes = t.chars().filter(|&ch| ch == '-').count();
        dashes >= 3 && t.chars().all(|ch| ch == '-')
    })
}

fn split_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

fn pad_visible(text: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(text);
    if w >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}

fn longest_word_width(text: &str, cap: usize) -> usize {
    let mut max = 1usize;
    let mut any_word = false;
    for word in text.split_whitespace() {
        any_word = true;
        max = max.max(UnicodeWidthStr::width(word).min(cap));
    }
    // No whitespace (e.g. CJK runs): treat whole cell as one word, capped.
    if !any_word && !text.is_empty() {
        max = UnicodeWidthStr::width(text).min(cap).max(1);
    }
    max.max(1).min(cap)
}

/// Allocate column widths like omp `#renderTable` (natural → min-word → shrink).
fn allocate_column_widths(header: &[String], rows: &[Vec<String>], available_width: usize) -> Option<Vec<usize>> {
    let num_cols = header.len();
    if num_cols == 0 {
        return None;
    }
    // "│ " + (n-1)*" │ " + " │" = 3n + 1
    let border_overhead = 3 * num_cols + 1;
    let available_for_cells = available_width.saturating_sub(border_overhead);
    if available_for_cells < num_cols {
        return None;
    }

    let mut natural = vec![0usize; num_cols];
    let mut min_word = vec![1usize; num_cols];

    let measure = |cells: &[String], natural: &mut [usize], min_word: &mut [usize]| {
        for (i, cell) in cells.iter().enumerate().take(num_cols) {
            let w = UnicodeWidthStr::width(cell.as_str());
            natural[i] = natural[i].max(w);
            min_word[i] = min_word[i].max(longest_word_width(cell, TABLE_MAX_WORD));
        }
    };
    measure(header, &mut natural, &mut min_word);
    for row in rows {
        measure(row, &mut natural, &mut min_word);
    }

    let mut min_cols = min_word.clone();
    let mut min_cells: usize = min_cols.iter().sum();

    if min_cells > available_for_cells {
        min_cols = vec![1; num_cols];
        let remaining = available_for_cells.saturating_sub(num_cols);
        if remaining > 0 {
            let total_weight: usize = min_word.iter().map(|w| w.saturating_sub(1)).sum();
            let mut growth = vec![0usize; num_cols];
            for i in 0..num_cols {
                let weight = min_word[i].saturating_sub(1);
                growth[i] = if total_weight > 0 {
                    (weight * remaining) / total_weight
                } else {
                    0
                };
                min_cols[i] += growth[i];
            }
            let allocated: usize = growth.iter().sum();
            let mut leftover = remaining.saturating_sub(allocated);
            let mut i = 0;
            while leftover > 0 && i < num_cols {
                min_cols[i] += 1;
                leftover -= 1;
                i += 1;
            }
        }
        min_cells = min_cols.iter().sum();
    }

    let total_natural: usize = natural.iter().sum::<usize>() + border_overhead;
    let column_widths = if total_natural <= available_width {
        natural
            .iter()
            .zip(min_cols.iter())
            .map(|(n, m)| (*n).max(*m))
            .collect()
    } else {
        let total_grow: usize = natural
            .iter()
            .zip(min_cols.iter())
            .map(|(n, m)| n.saturating_sub(*m))
            .sum();
        let extra = available_for_cells.saturating_sub(min_cells);
        let mut widths: Vec<usize> = min_cols
            .iter()
            .enumerate()
            .map(|(i, min_w)| {
                let delta = natural[i].saturating_sub(*min_w);
                let grow = if total_grow > 0 {
                    (delta * extra) / total_grow
                } else {
                    0
                };
                min_w + grow
            })
            .collect();
        let allocated: usize = widths.iter().sum();
        let mut remaining = available_for_cells.saturating_sub(allocated);
        while remaining > 0 {
            let mut grew = false;
            for i in 0..num_cols {
                if remaining == 0 {
                    break;
                }
                if widths[i] < natural[i] {
                    widths[i] += 1;
                    remaining -= 1;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        widths
    };
    Some(column_widths)
}

fn normalize_table(header: &[String], rows: &[Vec<String>]) -> (Vec<String>, Vec<Vec<String>>) {
    let n = header.len().max(
        rows.iter()
            .map(|r| r.len())
            .max()
            .unwrap_or(0),
    ).max(1);
    let pad = |cells: &[String]| -> Vec<String> {
        let mut v = cells.to_vec();
        while v.len() < n {
            v.push(String::new());
        }
        if v.len() > n {
            v.truncate(n);
        }
        v
    };
    (pad(header), rows.iter().map(|r| pad(r)).collect())
}

/// Strip inline markdown markers so measure/wrap/pad use the same visible text
/// that will be painted (avoids column drift / "ghost" │ inside cells).
fn cell_to_plain(raw: &str) -> String {
    parse_inline(raw, MdKind::Normal)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// Fit `text` into exactly `width` display columns (truncate + pad).
fn fit_cell(text: &str, width: usize) -> String {
    let width = width.max(1);
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
        if w + cw > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    pad_visible(&out, width)
}

fn push_table_data_line(out: &mut Vec<MdLine>, cells: &[String], kind: MdKind, col_widths: &[usize]) {
    let v = '│';
    let mut spans = vec![MdSpan {
        text: format!("{v} "),
        kind: MdKind::Dim,
    }];
    for (ci, text) in cells.iter().enumerate() {
        spans.push(MdSpan {
            text: fit_cell(text, col_widths[ci]),
            kind,
        });
        if ci + 1 < col_widths.len() {
            spans.push(MdSpan {
                text: format!(" {v} "),
                kind: MdKind::Dim,
            });
        }
    }
    spans.push(MdSpan {
        text: format!(" {v}"),
        kind: MdKind::Dim,
    });
    out.push(MdLine { spans });
}

fn render_table(header: &[String], rows: &[Vec<String>], width: usize) -> Vec<MdLine> {
    let width = width.max(1);
    let (header, rows) = normalize_table(header, rows);
    // omp measures after inline render — do the same before layout.
    let header: Vec<String> = header.iter().map(|c| cell_to_plain(c)).collect();
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| cell_to_plain(c)).collect())
        .collect();

    let Some(col_widths) = allocate_column_widths(&header, &rows, width) else {
        // Too narrow: fall back to raw pipes (wrapped), like omp.
        let mut raw = String::new();
        raw.push_str(&format!("| {} |\n", header.join(" | ")));
        raw.push_str(&format!(
            "| {} |\n",
            header
                .iter()
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        for row in &rows {
            raw.push_str(&format!("| {} |\n", row.join(" | ")));
        }
        return wrap_text(raw.trim_end(), width)
            .into_iter()
            .map(|w| MdLine::plain(w, MdKind::Dim))
            .collect();
    };

    let h = '─';
    let mut out = Vec::new();

    let border_line = |left: char, mid: char, right: char| -> String {
        let mut s = String::new();
        s.push(left);
        for (i, w) in col_widths.iter().enumerate() {
            s.push(h);
            s.push_str(&h.to_string().repeat(*w));
            s.push(h);
            if i + 1 < col_widths.len() {
                s.push(mid);
            }
        }
        s.push(right);
        s
    };

    out.push(MdLine::plain(border_line('┌', '┬', '┐'), MdKind::Dim));

    let header_wrapped: Vec<Vec<String>> = header
        .iter()
        .enumerate()
        .map(|(i, cell)| wrap_text(cell, col_widths[i].max(1)))
        .collect();
    let header_lines = header_wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
    for li in 0..header_lines {
        let cells: Vec<String> = header_wrapped
            .iter()
            .map(|c| c.get(li).cloned().unwrap_or_default())
            .collect();
        push_table_data_line(&mut out, &cells, MdKind::Bold, &col_widths);
    }

    let sep = border_line('├', '┼', '┤');
    out.push(MdLine::plain(sep.clone(), MdKind::Dim));

    for (ri, row) in rows.iter().enumerate() {
        let wrapped: Vec<Vec<String>> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| wrap_text(cell, col_widths[i].max(1)))
            .collect();
        let nlines = wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
        for li in 0..nlines {
            let cells: Vec<String> = wrapped
                .iter()
                .map(|c| c.get(li).cloned().unwrap_or_default())
                .collect();
            push_table_data_line(&mut out, &cells, MdKind::Normal, &col_widths);
        }
        if ri + 1 < rows.len() {
            out.push(MdLine::plain(sep.clone(), MdKind::Dim));
        }
    }

    out.push(MdLine::plain(border_line('└', '┴', '┘'), MdKind::Dim));
    out.push(MdLine::empty());

    // Defensive: every structural line must share the same visible width.
    debug_assert!({
        let widths: Vec<usize> = out
            .iter()
            .filter(|l| {
                let t: String = l.spans.iter().map(|s| s.text.as_str()).collect();
                t.contains('│') || t.contains('┌') || t.contains('└') || t.contains('├')
            })
            .map(|l| {
                let t: String = l.spans.iter().map(|s| s.text.as_str()).collect();
                UnicodeWidthStr::width(t.as_str())
            })
            .collect();
        widths.windows(2).all(|w| w[0] == w[1])
    });

    out
}

/// Prefixed block (list item): marker on first wrapped line, indent after;
/// body runs through [`parse_inline`] so `**` / `*` markers are not painted.
fn render_prefix_lines(
    marker: &str,
    marker_kind: MdKind,
    text: &str,
    inner_w: usize,
    indent_w: usize,
) -> Vec<MdLine> {
    let mut out = Vec::new();
    for (i, w) in wrap_text(text, inner_w).into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            spans.push(MdSpan {
                text: marker.to_string(),
                kind: marker_kind,
            });
        } else {
            spans.push(MdSpan {
                text: " ".repeat(indent_w),
                kind: MdKind::Normal,
            });
        }
        spans.extend(parse_inline(&w, MdKind::Normal));
        out.push(MdLine { spans });
    }
    out
}

/// Combine outer style `base` with an inner matched `kind`.
fn elevate(base: MdKind, kind: MdKind) -> MdKind {
    match kind {
        // Semantic colors win over bold/italic wrapper.
        MdKind::Code | MdKind::Link | MdKind::Heading | MdKind::CodeBlock => kind,
        MdKind::Normal => base,
        MdKind::Bold | MdKind::Italic => match base {
            MdKind::Bold | MdKind::Italic => MdKind::Bold,
            _ => kind,
        },
        other => other,
    }
}

/// Parse inline markers (`**bold**`, `*italic*`, `` `code` ``, `[text](url)`)
/// into styled spans; non-marker text uses `base`. Bold/italic recurse so
/// nested `` `code` `` is highlighted and backticks are stripped.
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
            match kind {
                MdKind::Bold | MdKind::Italic => {
                    let nested_base = elevate(base, kind);
                    for s in parse_inline(&content, nested_base) {
                        spans.push(MdSpan {
                            text: s.text,
                            kind: elevate(base, s.kind),
                        });
                    }
                }
                other => {
                    spans.push(MdSpan {
                        text: content,
                        kind: elevate(base, other),
                    });
                }
            }
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
    // `code` before emphasis so `` `**x**` `` stays code.
    if rest.starts_with('`') {
        if let Some(end) = rest[1..].find('`') {
            return Some((rest[1..1 + end].to_string(), MdKind::Code, 1 + end + 1));
        }
    }
    // **bold** / __bold__
    if rest.starts_with("**") {
        if let Some(end) = rest[2..].find("**") {
            return Some((rest[2..2 + end].to_string(), MdKind::Bold, 2 + end + 2));
        }
    }
    if rest.starts_with("__") {
        if let Some(end) = rest[2..].find("__") {
            return Some((rest[2..2 + end].to_string(), MdKind::Bold, 2 + end + 2));
        }
    }
    // *italic* (single star, not **). GFM-ish: opening * cannot be followed by
    // whitespace (so `* **bold**` / list-like stars are not eaten as italic).
    if rest.starts_with('*') && !rest.starts_with("**") {
        let after = &rest[1..];
        if after
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace())
        {
            if let Some(end) = find_single_delim(after, '*') {
                return Some((after[..end].to_string(), MdKind::Italic, 1 + end + 1));
            }
        }
    }
    // _italic_ (not part of __)
    if rest.starts_with('_') && !rest.starts_with("__") {
        let after = &rest[1..];
        if after
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace())
        {
            if let Some(end) = find_single_delim(after, '_') {
                return Some((after[..end].to_string(), MdKind::Italic, 1 + end + 1));
            }
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

/// Index of a single-character closer that is not part of a double delimiter
/// (`**` / `__`).
fn find_single_delim(after: &str, delim: char) -> Option<usize> {
    let mut i = 0;
    while i < after.len() {
        if after[i..].starts_with(delim) {
            // Skip `**` / `__` pairs so italic closer isn't the first half of bold.
            if delim == '*' && after[i..].starts_with("**") {
                i += 2;
                continue;
            }
            if delim == '_' && after[i..].starts_with("__") {
                i += 2;
                continue;
            }
            // Closing delimiter must not be preceded by whitespace (GFM-ish).
            if i > 0 {
                let prev = after[..i].chars().last();
                if prev.is_some_and(|c| c.is_whitespace()) {
                    i += delim.len_utf8();
                    continue;
                }
            }
            return Some(i);
        }
        let ch = after[i..].chars().next()?;
        i += ch.len_utf8();
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
    fn nested_code_inside_bold_strips_backticks() {
        let lines = render_markdown("1. **`.cursor/worktrees.json` 自动 setup**", 80);
        let t = plain_text(&lines[0]);
        assert!(t.contains(".cursor/worktrees.json"), "{t}");
        assert!(!t.contains('`'), "backticks leaked: {t}");
        assert!(!t.contains('*'), "stars leaked: {t}");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.kind == MdKind::Code && s.text.contains("worktrees")),
            "{:?}",
            lines[0].spans
        );
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
    fn list_item_strips_bold_markers() {
        let lines = render_markdown("* **Cursor 路线**: 说明", 80);
        let t = plain_text(&lines[0]);
        assert!(t.starts_with('•'), "{t}");
        assert!(t.contains("Cursor 路线"), "{t}");
        assert!(!t.contains('*'), "markers leaked: {t}");
        assert!(
            lines[0].spans.iter().any(|s| s.kind == MdKind::Bold && s.text.contains("Cursor")),
            "{:?}",
            lines[0].spans
        );
    }

    #[test]
    fn ordered_list_strips_bold_markers() {
        let lines = render_markdown("1. **自动清理 + 限额** — 说明", 80);
        let t = plain_text(&lines[0]);
        assert!(t.starts_with("1. "), "{t}");
        assert!(t.contains("自动清理"), "{t}");
        assert!(!t.contains('*'), "markers leaked: {t}");
    }

    #[test]
    fn star_space_bold_not_eaten_as_italic() {
        // Paragraph form: leading "* **" must not become italic-of-space.
        let lines = render_markdown("see * **Bold** ok", 80);
        let t = plain_text(&lines[0]);
        assert!(t.contains("Bold"), "{t}");
        assert!(!t.contains("**"), "{t}");
        // Leading list-like star before bold stays a literal '*'.
        assert!(t.contains("* Bold") || t.contains("*Bold") || t.contains("see *"), "{t}");
    }

    #[test]
    fn star_count_not_italic() {
        let lines = render_markdown("agent-deck (Go, 643*, fleet)", 80);
        let t = plain_text(&lines[0]);
        assert!(t.contains("643*"), "{t}");
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

    #[test]
    fn gfm_table_renders_box_borders() {
        let src = "\
| 维度 | Cursor | dux |
| --- | --- | --- |
| A | yes | no |
| B | ✅ | ❌ |
";
        let lines = render_markdown(src, 80);
        let joined: String = lines.iter().map(plain_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains('┌'), "top border: {joined}");
        assert!(joined.contains('└'), "bottom border: {joined}");
        assert!(joined.contains('┼') || joined.contains('├'), "separator: {joined}");
        assert!(joined.contains("维度"), "{joined}");
        assert!(joined.contains("Cursor"), "{joined}");
        // Must not leave raw markdown separator visible as primary render.
        assert!(!joined.contains("| --- |"), "{joined}");
    }

    #[test]
    fn gfm_table_columns_align_with_cjk() {
        let src = "\
| 维度 | Cursor | dux |
| --- | --- | --- |
| worktree 定位 | 隔离执行 | git 舞台 |
";
        let lines = render_markdown(src, 60);
        // Every border/data line that contains │ should share the same visible width.
        let widths: Vec<usize> = lines
            .iter()
            .map(plain_text)
            .filter(|s| s.contains('│') || s.contains('┌') || s.contains('└') || s.contains('├'))
            .map(|s| UnicodeWidthStr::width(s.as_str()))
            .collect();
        assert!(!widths.is_empty());
        let first = widths[0];
        assert!(
            widths.iter().all(|&w| w == first),
            "mismatched table line widths: {widths:?}"
        );
    }

    #[test]
    fn gfm_table_inline_markers_do_not_shift_columns() {
        // Markers must be stripped before measure/pad, otherwise │ drifts.
        let src = "\
| A | B |
| --- | --- |
| `code` x | **bold** y |
| plain | plain |
";
        let lines = render_markdown(src, 40);
        let widths: Vec<usize> = lines
            .iter()
            .map(plain_text)
            .filter(|s| s.contains('│') || s.contains('┌'))
            .map(|s| UnicodeWidthStr::width(s.as_str()))
            .collect();
        let first = widths[0];
        assert!(
            widths.iter().all(|&w| w == first),
            "marker-induced drift: {widths:?}\n{}",
            lines.iter().map(plain_text).collect::<Vec<_>>().join("\n")
        );
        let joined = lines.iter().map(plain_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("code"), "{joined}");
        assert!(!joined.contains('`'), "{joined}");
    }

    #[test]
    fn gfm_table_adapts_to_narrow_width() {
        let src = "\
| 维度 | Cursor | dux |
| --- | --- | --- |
| 多模型竞速 | ✅ /best-of-n 同任务多模型各一 worktree | ❌ 无竞速概念 |
";
        let wide = render_markdown(src, 100);
        let narrow = render_markdown(src, 40);
        let wide_s: String = wide.iter().map(plain_text).collect::<Vec<_>>().join("\n");
        let narrow_s: String = narrow.iter().map(plain_text).collect::<Vec<_>>().join("\n");
        // Narrow render should still be bounded and include content.
        assert!(narrow.iter().all(|l| UnicodeWidthStr::width(plain_text(l).as_str()) <= 40));
        assert!(narrow_s.contains("多模型") || narrow_s.contains("竞速") || narrow_s.contains('|'));
        // Wide prefers boxed form when it fits.
        assert!(wide_s.contains('┌') || wide_s.contains('│'));
    }
}
