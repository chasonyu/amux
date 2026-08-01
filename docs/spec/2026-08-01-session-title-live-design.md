# Session title live display (amux sidebar)

Date: 2026-08-01  
Status: approved → implementing

## Goal

Sidebar session names update near-realtime for:

1. **Provisional** — first user message (omp does not persist this as title)
2. **Official** — LLM auto title or `/rename` (JSONL fixed 256-byte title slot)
3. **Fallback** — `New session` (live synthetic) or short id (disk)

Latency target for official title changes: ≤1s on local disk (fsnotify primary).

## Non-goals

- No omp changes / no PTY OSC scraping
- Watch only the **current** workspace session directory
- No auto vs user style distinction in v1

## Display priority

`official title slot nonempty` → `sanitized firstMessage` → fallback

Provisional titles render dim (`hint_desc_fg`); official/fallback use normal session text style.

## Read path

| Source | How |
|--------|-----|
| Official | Read first 256 UTF-8 bytes (omp `SESSION_TITLE_SLOT_BYTES`), parse `type:"title"` |
| Provisional | If slot empty: stream JSONL after slot (cap ~64KB / 200 lines), first `type:"message"` with `role:"user"` text |
| Fingerprint | `(mtime, size)` — skip re-parse when unchanged |

## Refresh

- `notify` watch on `~/.omp/agent/sessions/<cwd-key>/`
- Debounce ~80ms; MODIFY of known `.jsonl` → title-only refresh; CREATE/DELETE/overflow → full list reconcile
- Fallback poll every 3s comparing fingerprints
- Watch setup failure → 1s mtime poll only

## Acceptance

1. New session: `New session` → provisional after first user msg lands → official ≤1s after LLM/rename  
2. Workspace switch does not leak titles across dirs  
3. Watch unavailable still updates (slower)
