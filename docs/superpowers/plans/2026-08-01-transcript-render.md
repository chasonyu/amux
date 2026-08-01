# Disk Transcript Render + Theme Appearance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make non-live Agent-pane previews look like omp’s default collapsed transcript, with OSC 11 dark/light themes for chrome + preview, using a provider-neutral block model (omp first).

**Architecture:** Parse session files per `provider` into `Vec<TranscriptBlock>`; shared `render(blocks, width, &Theme)` produces ratatui lines. `Theme::dark()` / `Theme::light()` selected via host OSC 11 luminance (fail → dark). No expand keybindings.

**Tech Stack:** Rust, ratatui, existing amux `provider` / `shell` / `theme`; oh-my-pi as visual reference only (no Node sidecar).

**Spec:** `docs/superpowers/specs/2026-08-01-transcript-render-design.md`

**Note:** amux already uses `#[cfg(test)]` unit tests; follow TDD here. Git commits use `/usr/bin/git commit -F /tmp/commit-msg.txt` (Git 2.19, no `--trailer`).

---

## File map

| Path | Role |
|------|------|
| `src/appearance.rs` | OSC 11 probe + luminance → `Appearance` |
| `src/theme.rs` | `Theme::dark()` / `light()`; `Default` → dark; transcript color slots |
| `src/provider/transcript/mod.rs` | `TranscriptBlock`, `load(provider, path)` dispatch |
| `src/provider/transcript/omp.rs` | omp jsonl → blocks |
| `src/provider/transcript/markdown.rs` | lightweight MD → plain lines (+ style hints) |
| `src/provider/transcript/render.rs` | blocks → `Vec<TranscriptLine>` / ratatui helpers |
| `src/provider/transcript.rs` | **remove** after move (or thin re-export during migrate) |
| `src/provider/mod.rs` | export new API |
| `src/lib.rs` | `mod appearance` |
| `src/shell/mod.rs` | probe theme at start; use `load` + new render; cache blocks |
| `src/main.rs` | if needed, pass stdout for probe |
| Fixtures under `src/provider/transcript/fixtures/` | tiny jsonl samples |

Constants (collapsed, from omp `PREVIEW_LIMITS` / `TRUNCATE_LENGTHS`):

```rust
pub const COLLAPSED_LINES: usize = 3;
pub const OUTPUT_COLLAPSED: usize = 3;
pub const COLLAPSED_ITEMS: usize = 8;
pub const TRUNCATE_LINE: usize = 110;
pub const TRUNCATE_TITLE: usize = 60;
pub const TRUNCATE_ARG: usize = 100;
```

---

### Task 1: Appearance classification (no I/O)

**Files:**
- Create: `src/appearance.rs`
- Modify: `src/lib.rs` (add `pub mod appearance;`)

- [ ] **Step 1: Write failing tests in `appearance.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_when_luma_low() {
        assert_eq!(appearance_from_rgb(0x22, 0x1d, 0x1a), Appearance::Dark);
        assert_eq!(appearance_from_rgb(0, 0, 0), Appearance::Dark);
    }

    #[test]
    fn light_when_luma_high() {
        assert_eq!(appearance_from_rgb(0xf5, 0xf5, 0xf5), Appearance::Light);
        assert_eq!(appearance_from_rgb(255, 255, 255), Appearance::Light);
    }

    #[test]
    fn parse_osc11_rgb() {
        let s = "\x1b]11;rgb:221d/1d1d/1a1a\x07";
        let (r, g, b) = parse_osc11_rgb(s).unwrap();
        assert!(r < 0x40 && g < 0x40);
    }
}
```

- [ ] **Step 2: Run tests (expect fail)**

```bash
cd opensource/amux && cargo test --lib appearance::
```

Expected: compile error / FAIL (module missing).

- [ ] **Step 3: Implement**

```rust
//! Host terminal appearance (OSC 11).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// BT.601 luma threshold: omp uses bg < 8 on 0..15 scale ≈ mid-gray.
/// We use 0..255 channel avg: `< 128` → Dark.
pub fn appearance_from_rgb(r: u8, g: u8, b: u8) -> Appearance {
    let y = (r as u16 * 299 + g as u16 * 587 + b as u16 * 114) / 1000;
    if y < 128 {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

pub fn parse_osc11_rgb(reply: &str) -> Option<(u8, u8, u8)> {
    // Accept rgb:RRRR/GGGG/BBBB (16-bit per channel) or rgb:RR/GG/BB
    let start = reply.find("rgb:")? + 4;
    let body = reply[start..]
        .split(|c| c == '\x07' || c == '\x1b')
        .next()?
        .trim();
    let mut parts = body.split('/');
    let r = parse_osc_channel(parts.next()?)?;
    let g = parse_osc_channel(parts.next()?)?;
    let b = parse_osc_channel(parts.next()?)?;
    Some((r, g, b))
}

fn parse_osc_channel(s: &str) -> Option<u8> {
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(match s.len() {
        2 => v as u8,
        4 => (v >> 8) as u8,
        _ => return None,
    })
}
```

Also implement `probe_appearance(out: &mut impl Write, input: &mut impl Read) -> Appearance` that writes `\x1b]11;?\x07` then DA1 `\x1b[c`, reads with short timeout (~200ms), parses; on failure returns `Dark`. Keep probe logic small; may live in same file.

- [ ] **Step 4: `cargo test --lib appearance::` → PASS**

- [ ] **Step 5: Commit**

```bash
# message via -F file; include AI tracking lines
```

---

### Task 2: Theme dark / light presets

**Files:**
- Modify: `src/theme.rs`

- [ ] **Step 1: Failing test**

```rust
#[test]
fn light_theme_has_light_app_bg() {
    let t = Theme::light();
    // app_bg should be bright-ish (at least one channel high)
    match t.app_bg {
        Color::Rgb(r, g, b) => assert!(r as u16 + g as u16 + b as u16 > 500),
        _ => {}
    }
    assert_ne!(Theme::dark().transcript_user_bg, Theme::light().transcript_user_bg);
}
```

- [ ] **Step 2: Run → FAIL**

- [ ] **Step 3: Implement `Theme::dark()` / `Theme::light()`**

- Move current `Default` body into `Theme::dark()`.
- `impl Default for Theme { fn default() -> Self { Self::dark() } }`
- `Theme::light()`: light chrome (near-white `app_bg`, dark text); transcript slots from omp `light.json` semantic colors (`userMessageBg` / text / thinking / tool) — approximate with RGB; prefer Indexed for overlays if needed for WebSSH.
- Add `Theme::for_appearance(Appearance)`.

- [ ] **Step 4: Tests PASS**

- [ ] **Step 5: Commit**

---

### Task 3: Wire OSC 11 probe into App startup

**Files:**
- Modify: `src/shell/mod.rs` (`App::new` / `run`)
- Modify: `src/main.rs` if `run` needs raw stdout before alt screen

- [ ] **Step 1: Manual/integration check plan** (no flaky TTY test in CI)

Document in code comment: probe **before** `EnterAlternateScreen` when possible; if already in alt screen, still query once.

- [ ] **Step 2: In `App::new` or `run` entry**

```rust
let appearance = crate::appearance::probe_appearance(&mut stdout, &mut stdin)
    .unwrap_or(Appearance::Dark);
let theme = Theme::for_appearance(appearance);
```

Store `appearance` on `App` if useful for later Mode 2031 (optional this task: skip 2031 unless trivial).

- [ ] **Step 3: `cargo build --release` + smoke run amux once** — chrome readable on current terminal

- [ ] **Step 4: Commit**

---

### Task 4: Neutral `TranscriptBlock` + load dispatch

**Files:**
- Create: `src/provider/transcript/mod.rs`
- Create: `src/provider/transcript/omp.rs` (stub parse → empty / migrate next)
- Modify: `src/provider/mod.rs` — `pub mod transcript;` re-exports
- Keep old `src/provider/transcript.rs` temporarily as `transcript_legacy` OR delete after Task 5

Block enum (agent-neutral):

```rust
#[derive(Debug, Clone)]
pub enum TranscriptBlock {
    User { text: String, synthetic: bool },
    Assistant { text: String },
    Thinking { summary: String }, // already one-line; body discarded at parse
    Tool {
        name: String,
        title: String,
        status: ToolStatus, // Pending / Ok / Err
        arg_preview: Vec<String>,
        output_preview: Vec<String>,
        kind: ToolKind, // Default / Read / Bash / Eval
    },
    ReadGroup { paths: Vec<String>, status: ToolStatus },
    Meta { text: String }, // compaction divider, etc.
    Spacer,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolStatus { Pending, Ok, Error }

#[derive(Debug, Clone, Copy)]
pub enum ToolKind { Default, Read, Bash, Eval }
```

```rust
pub fn load_transcript(provider: &str, path: &Path) -> Vec<TranscriptBlock> {
    match provider {
        "omp" => omp::load(path),
        other => vec![TranscriptBlock::Meta {
            text: format!("(no transcript preview for provider `{other}`)"),
        }],
    }
}
```

- [ ] **Step 1: Test unknown provider**

```rust
#[test]
fn unknown_provider_placeholder() {
    let b = load_transcript("other", Path::new("/nope"));
    assert!(matches!(&b[0], TranscriptBlock::Meta { text } if text.contains("other")));
}
```

- [ ] **Step 2–4: Implement mod + PASS + Commit**

---

### Task 5: omp jsonl → blocks (parity with timeline)

**Files:**
- Implement: `src/provider/transcript/omp.rs`
- Create: `src/provider/transcript/fixtures/sample_turn.jsonl` (minimal user + assistant text + toolCall + toolResult + thinking)
- Remove/replace old `src/provider/transcript.rs` parsers; update `shell` imports in Task 7

Rules (from spec / omp):

1. Skip title slot / non-message noise (reuse `skip_title_prefix`).
2. `user` / non-synthetic → `User`; synthetic/developer → `User { synthetic: true }` (render dim later).
3. `assistant`: split with tool timeline — text/thinking before tools, then tools, then after-tool text (`splitAssistantMessageToolTimeline` logic).
4. `thinking` content → `Thinking { summary: one_line(...) }` only (no body).
5. `toolCall` + later `toolResult` merge into one `Tool` (match id).
6. Consecutive `read` tools → `ReadGroup` (collapse).
7. `bash` / `eval` / names containing bash → `ToolKind::Bash` / `Eval`.
8. `compaction` / `branch_summary` → `Meta` divider text.
9. Caps: `MAX_READ_BYTES`, `MAX_BLOCKS`.

- [ ] **Step 1: Fixture + failing parse test**

```rust
#[test]
fn parses_user_assistant_tool() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/provider/transcript/fixtures/sample_turn.jsonl");
    let blocks = omp::load(&path);
    assert!(blocks.iter().any(|b| matches!(b, TranscriptBlock::User { .. })));
    assert!(blocks.iter().any(|b| matches!(b, TranscriptBlock::Tool { .. })));
}
```

- [ ] **Step 2: FAIL → implement → PASS → Commit**

Port useful helpers from old `transcript.rs` (`content_to_text`, `format_primary_arg`) into `omp.rs`.

---

### Task 6: Lightweight markdown

**Files:**
- Create: `src/provider/transcript/markdown.rs`

Support only: ATX headings (`#`), unordered `- `/`* ` lists, fenced ` ``` `, `**bold**` / `*italic*` (simple), paragraphs, wrap to width. Output: `Vec<MdLine { text, kind }>` where kind is Normal / Heading / Code / List / Dim.

- [ ] **Step 1: Test**

```rust
#[test]
fn fences_and_heading() {
    let lines = render_markdown("# Hi\n\n```rs\nlet x=1;\n```\n", 40);
    assert!(lines.iter().any(|l| l.text.contains("Hi")));
    assert!(lines.iter().any(|l| l.text.contains("```")));
}
```

- [ ] **Step 2–4: Implement → PASS → Commit**

No new crates unless already present; hand-roll is fine for v1.

---

### Task 7: Block renderer (collapsed) + shell wiring

**Files:**
- Create: `src/provider/transcript/render.rs`
- Modify: `src/shell/mod.rs` — `TranscriptCache` stores `Vec<TranscriptBlock>` + rendered lines; `draw_transcript_preview` uses render; pass `summary.provider`
- Delete obsolete line-based API or keep thin wrapper

Render rules:

- `Spacer` / inter-block: ensure single blank between blocks (container strips edges).
- `User`: each MD line with `transcript_user_bg/fg`, horizontal pad 1 space.
- `Assistant`: MD with assistant fg, pad 1, no bg.
- `Thinking`: one italic/dim line e.g. `Thinking` or summary (≤ 80 chars).
- `Tool` Default: `✔ title` / `✘` / `⏳`; then up to `COLLAPSED_LINES` of ` └─ ` / ` ├─ ` for args+output (`OUTPUT_COLLAPSED`).
- `ReadGroup`: `✔ Read · N files` + up to `COLLAPSED_ITEMS` paths as tree lines.
- `Bash`/`Eval`: top/bottom full-width `─` (use area width); header `$ cmd` or `>>>`.
- `Meta`: dim divider.
- Optional static suffix on truncated tools: ` (ctrl+o: Expand)` in meta fg — **do not** handle the key.

- [ ] **Step 1: Snapshot-style test**

```rust
#[test]
fn tool_card_has_tree_prefix() {
    let theme = Theme::dark();
    let blocks = vec![TranscriptBlock::Tool {
        name: "read".into(),
        title: "Read: foo.rs".into(),
        status: ToolStatus::Ok,
        arg_preview: vec!["foo.rs".into()],
        output_preview: vec!["fn main() {}".into()],
        kind: ToolKind::Default,
    }];
    let lines = render_blocks(&blocks, 60, &theme);
    let joined = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("Read"));
    assert!(joined.contains("└─") || joined.contains("├─"));
}
```

(Define `RenderedLine { role, text }` for tests; shell maps to ratatui.)

- [ ] **Step 2–3: Implement render + update shell**

```rust
// shell cache
struct TranscriptCache {
    path: PathBuf,
    mtime: DateTime<Utc>,
    size: u64,
    blocks: Vec<TranscriptBlock>,
}
// on draw: render_blocks(&cache.blocks, area.width, &self.theme)
// load: load_transcript(provider, path)
```

- [ ] **Step 4: `cargo test --lib && cargo build --release`**

- [ ] **Step 5: Commit**

---

### Task 8: Polish + manual parity check

**Files:**
- Adjust colors / spacing against a real omp session jsonl
- Update Help note if needed: preview is display-only
- Update `docs/spec/2026-08-01-jsonl-transcript-preview-design.md` status line → superseded by new spec (one sentence)

- [ ] **Step 1: Manual** — pick a session with tools+thinking; compare amux Nav preview vs omp look (collapsed)
- [ ] **Step 2: Fix obvious gaps (pad, blank lines, read group)**
- [ ] **Step 3: `cargo test --lib && cargo build --release`**
- [ ] **Step 4: Commit** (`feat: omp-faithful transcript preview + appearance themes`)

---

## Spec coverage checklist

| Spec item | Task |
|-----------|------|
| Multi-agent load dispatch | 4 |
| omp parser + tool timeline + read group + bash | 5 |
| Thinking one-line | 5 + 7 |
| Markdown subset | 6 |
| Collapsed tool cards / limits | 7 |
| No expand keys | 7 (explicit) |
| OSC 11 dark/light chrome+transcript | 1–3 |
| Cache re-render on theme | 7 (blocks cached) |
| Caps / unknown provider | 4–5 |
| Unit tests | 1,2,4,5,6,7 |

## Self-review notes

- No sidecar / no `ctrl+o` handling.
- Types named `TranscriptBlock` / `ToolKind` consistently across tasks.
- Mode 2031 optional; not required for Done.
- Do not introduce npm/bun dependency.
