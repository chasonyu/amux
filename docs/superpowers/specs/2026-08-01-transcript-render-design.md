# amux Disk Transcript Render (omp-faithful, multi-agent ready)

Date: 2026-08-01  
Status: approved for planning  
Supersedes visual ambition of: `docs/spec/2026-08-01-jsonl-transcript-preview-design.md` (keep that doc for history; this is the target)

## Goal

When a session is **not live**, the Agent pane shows a disk-backed transcript preview that **looks as close as practical to omp’s default collapsed TUI**, without embedding omp/Node.

amux remains **agent-agnostic**: omp is the first provider implementation, not the identity of the preview stack.

## Non-goals

- Interactive expand/collapse (`ctrl+o` / per-card expand). Display omp’s **default collapsed** look only; static expand-hint text is optional decoration and must not require key handling.
- Pixel-perfect markdown (no mermaid, no full syntax-highlight engine required in v1).
- Implementing a second agent provider in this iteration (only leave clean extension points).
- Calling omp/pi-tui as a render sidecar.

## Decisions (from brainstorm)

| Topic | Choice |
|-------|--------|
| Approach | Rust in-process renderer; oh-my-pi source as golden visual/spec reference |
| Fidelity | Layout + Markdown subset + tool cards (read group, bash/eval frames, default tool tree) |
| Thinking | Default collapsed one-line summary (omp-like) |
| Expand keys | None |
| Theme | OSC 11 dark/light for **chrome + transcript** (fail → dark) |
| Multi-agent | Shared block model + draw path; per-provider parsers |

## Architecture

```
session.path + session.provider
        │
        ▼
provider::transcript::load(provider, path) ──► omp parser (first)
        │
        ▼
Vec<TranscriptBlock>          // agent-neutral
        │
        ▼
render(blocks, width, &Theme) // collapsed defaults
        │
        ▼
shell Agent pane (ratatui)
```

### Module layout (indicative)

- `provider/transcript/` (or equivalent split of today’s `transcript.rs`)
  - `mod.rs` — `TranscriptBlock`, `load(provider, path)`, dispatch
  - `render.rs` — blocks → display lines / ratatui `Line`s (theme-driven)
  - `markdown.rs` — lightweight subset (headings, lists, fenced code, bold/italic, plain wrap)
  - `omp.rs` (or `provider/omp/transcript.rs`) — jsonl → blocks (omp-specific)
- `theme.rs` — `Appearance::{Dark,Light}`, `Theme::dark()` / `Theme::light()`, transcript color slots
- Host OSC 11 probe at startup (DA1 sentinel pattern aligned with omp/pi-tui); optional Mode 2031 re-query when supported

Naming: prefer neutral types (`User`, `Assistant`, `Thinking`, `ToolCall`, `Meta`). Do not expose `omp_*` as the only public preview API.

## Visual rules (collapsed / display-only)

Aligned with omp coding-agent transcript components + dark/light theme JSON:

| Block | Appearance |
|-------|------------|
| User | Full-width bg bubble, ~1 cell pad, no role label, light MD |
| Assistant | No bg, left pad, MD (`codeBlockIndent` ≈ 0) |
| Thinking | Single summary line only (no body) |
| Tool (default) | Status icon + title; `└─` / `├─` preview; collapsed line limits from omp `PREVIEW_LIMITS` / render-utils |
| Read group | Collapse consecutive reads into one compact group |
| Bash / Eval | Full-width `─` rules + `$` / python-style header when detectable |
| Compaction / branch | Divider-style meta |
| Inter-block | Exactly one blank row between visible blocks |

Colors: port semantic roles from omp `dark.json` / `light.json` into `Theme` (Indexed preferred where WebSSH truecolor fails).

## Data / cache / limits

- Cache key: path + mtime + size (existing). Theme change → re-render from cached blocks, no disk re-read.
- Keep byte/line caps for huge jsonl; emit a meta truncation line.
- Skip bad jsonl lines; empty → `(no messages yet)`.
- Unknown `provider` → clear meta placeholder (no omp parser guess).

## Testing

- Unit: omp jsonl fixtures → `TranscriptBlock` sequence.
- Unit: blocks → plain/ANSI-ish line snapshots (no real TTY).
- Unit: OSC 11 luminance → Appearance classification (table-driven).
- Manual: side-by-side amux preview vs omp on same session (dark + light terminal if available).

## Reference sources (oh-my-pi)

- `packages/coding-agent/src/modes/components/*` (user, assistant, tool, bash, read-group, transcript-container)
- `packages/coding-agent/src/tools/default-renderer.ts`, `render-utils.ts`
- `packages/coding-agent/src/modes/theme/dark.json`, `light.json`
- `packages/tui/src/terminal.ts` (OSC 11 / Mode 2031)
- `packages/coding-agent/src/modes/utils/transcript-render-helpers.ts` (`splitAssistantMessageToolTimeline`)

## Out of scope follow-ups

- Interactive expand matching omp `ctrl+o`
- Full marked + highlight parity / mermaid
- Second agent parser
- Live PTY theme sync beyond host chrome (omp inside PTY already has its own theme)
