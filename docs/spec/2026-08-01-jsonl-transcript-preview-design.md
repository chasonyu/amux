# JSONL transcript preview (non-running sessions)

Date: 2026-08-01  
Status: implemented (MVP)

> Superseded by [docs/superpowers/specs/2026-08-01-transcript-render-design.md](../superpowers/specs/2026-08-01-transcript-render-design.md).

## Policy

| Session state | Agent pane |
|---------------|------------|
| Live + not exited | omp PTY snapshot (unchanged) |
| Disk / exited / no PTY | Parse `~/.omp/.../*.jsonl`, render omp-like transcript |

## Parity (omp interactive / history-format)

- Render `message`, `custom_message` (`display!==false`), `compaction`, `branch_summary`
- User: indented + background bubble; no `User:` label
- Assistant text: indented, no bubble
- toolCall → `⚙ name(arg)` + result summary; thinking dim italic one-liner
- Skip metadata entries (title slot, session, model_change, …)

## Non-goals (MVP)

- Full markdown / images / exact theme tokens
- Scroll keys (show tail that fits height)
- Live PTY grayscale while Nav-focused (optional later)
